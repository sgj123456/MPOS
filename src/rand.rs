//! The kernel's random source.
//!
//! Why this exists: `/dev/urandom` is the *only* entropy source a busybox
//! userland has - its TLS implementation reads that one file and nothing else,
//! not even `getrandom(2)` - so a kernel with no `/dev/urandom` cannot run an
//! HTTPS client however correct its TCP is. The same bytes back
//! `getrandom(2)`, so a program that reaches for either gets the same stream.
//!
//! What was here before was not a source but a gesture: a xorshift64 seeded
//! from the tick counter *on every call*. Two calls inside one tick return the
//! same bytes, so a TLS handshake's server-random and its key share could be
//! identical - not weak randomness, repeated randomness. This replaces it with
//! one ChaCha20 stream, seeded once from the machine and re-keyed on demand by
//! `RNDRESEEDCRNG` ([`reseed`]).

use core::cell::RefCell;

use critical_section::Mutex;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};

/// The generator, behind the usual kernel lock. `None` until the first use:
/// seeding touches the CPU and the clock, and neither is up as early as this
/// module is compiled.
static RNG: Mutex<RefCell<Option<ChaCha20Rng>>> = Mutex::new(RefCell::new(None));

/// Seed the generator. Idempotent, so a caller may ask at any point in the
/// boot without having to know whether it is the first to arrive.
pub fn init() {
    critical_section::with(|cs| {
        let mut slot = RNG.borrow(cs).borrow_mut();
        if slot.is_none() {
            *slot = Some(ChaCha20Rng::from_seed(seed()));
        }
    });
}

/// Fill `out` with random bytes.
///
/// Seeds on first use if [`init`] was never called: a syscall or a device read
/// should not have to know the boot order to get entropy.
pub fn fill(out: &mut [u8]) {
    critical_section::with(|cs| {
        let mut slot = RNG.borrow(cs).borrow_mut();
        let rng = slot.get_or_insert_with(|| ChaCha20Rng::from_seed(seed()));
        rng.fill_bytes(out);
    });
}

/// Re-key the generator from fresh seed material.
///
/// This is what `RNDRESEEDCRNG` asks for (Linux re-keys the CRNG from its input
/// pool). The generator has no pool to draw from, so the new key is built from
/// the two things it does have: a fresh [`seed`] - the hardware source, the
/// host's virtio-rng entropy and the boot instant, the same ingredients the
/// first key came from - and a draw from the *current* stream. Mixing the
/// current output in is what makes the re-key a real one rather than a reset:
/// a caller who had seen the first seed would otherwise predict the state a
/// bare `from_seed(seed())` restores, whereas the new key depends on every byte
/// the generator has produced so far. It also means a reseed is never a fixed
/// point even where `seed()` is (the host test build), which is what makes the
/// behaviour testable there.
///
/// The re-key is not a claim of cryptographic ratcheting - one XOR draws no
/// such conclusion - only that the state after the call is one no earlier
/// output determines.
pub fn reseed() {
    critical_section::with(|cs| {
        let mut slot = RNG.borrow(cs).borrow_mut();
        // Draw from the current generator before replacing it. `None` (never
        // seeded) contributes nothing and the fresh seed stands alone, which
        // is the same state `init` would have made.
        let mut carried = [0u8; 32];
        if let Some(rng) = slot.as_mut() {
            rng.fill_bytes(&mut carried);
        }
        let mut fresh = seed();
        for (byte, old) in fresh.iter_mut().zip(carried) {
            *byte ^= old;
        }
        *slot = Some(ChaCha20Rng::from_seed(fresh));
    });
}

/// A fresh seed: what the machine can be asked for, mixed with what nobody can
/// guess about it.
///
/// The hardware generator comes first when the CPU has one - on x86_64 that is
/// `RDRAND`, which QEMU emulates as well - and the boot instant is mixed in on
/// top of it in every case. A machine with no `RDRAND` still gets a seed that
/// an outside caller cannot predict, because when this kernel started is not
/// something a process on it can learn: the clock is under this kernel's
/// control, and reads of it return as much as the kernel chooses to say.
///
/// What this does not do is hold up boot waiting for the entropy pool to fill.
/// A machine that boots headless has no other events to gather from, and an
/// HTTPS client that waits forever for a better seed is worse than one that
/// starts - the seed here is not ideal, and it is not the same twice.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn seed() -> [u8; 32] {
    let mut seed = [0u8; 32];

    if let Some(rdrand) = x86_64::instructions::random::RdRand::new() {
        for chunk in seed.chunks_mut(8) {
            // The instruction reports "no value" rather than lying, and a
            // refusal leaves that stretch of the seed as it was - the clock
            // mix below still covers it.
            if let Some(value) = rdrand.get_u64() {
                chunk.copy_from_slice(&value.to_le_bytes());
            }
        }
    }

    // The host's own random source, when the machine has a virtio-rng
    // device: bytes no software on this machine chose, which is a stronger
    // seed than any of the three below. Mixed in rather than trusted alone -
    // a device that answers with a weak stream still cannot make the seed
    // worse than the machine's own sources, and XOR is what keeps that true.
    // The boot probes the device before `init` runs, so the entropy the
    // first caller gets is the entropy the machine had.
    if let Some(entropy) = crate::drivers::virtio::rng::entropy() {
        for (byte, host) in seed.iter_mut().zip(entropy) {
            *byte ^= host;
        }
    }

    // The boot instant and the tick counter, mixed one byte at a time so a
    // zero-filled stretch above is not left as zeroes.
    let epoch = crate::arch::rtc::boot_epoch();
    let ticks = crate::arch::timer::ticks();
    let stamp = epoch ^ ticks.rotate_left(32);
    for (i, byte) in seed.iter_mut().enumerate() {
        let word = if i % 2 == 0 { epoch } else { ticks ^ stamp };
        *byte ^= (word.rotate_left((i as u32 % 8) * 8) & 0xff) as u8;
    }
    seed
}

/// The seed for a build that is not the shipped kernel - the host test build,
/// and any architecture this module has no hardware source for. Deterministic
/// on purpose: a unit test that seeds differently every run is a unit test that
/// fails every other time.
#[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
fn seed() -> [u8; 32] {
    *b"mpos-host-seed: not a secret, te"
}

/// A read of the generator's *future* output that does not consume it: the
/// stream is advanced on a clone, so two calls with no `reseed` between them
/// return the same bytes. That is what a host test needs to see a re-key
/// without depending on how many other tests drew from the shared stream in
/// between. Test-only: no caller in the kernel has a use for it.
#[cfg(all(test, not(target_os = "none")))]
pub fn fingerprint() -> [u8; 32] {
    critical_section::with(|cs| {
        let mut slot = RNG.borrow(cs).borrow_mut();
        let rng = slot.get_or_insert_with(|| ChaCha20Rng::from_seed(seed()));
        let mut out = [0u8; 32];
        rng.clone().fill_bytes(&mut out);
        out
    })
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{fill, fingerprint, reseed};

    #[test]
    fn two_fills_do_not_repeat() {
        // The bug this module exists to fix: the old source re-seeded from the
        // tick on every call, so two calls in one tick returned the same bytes.
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        fill(&mut first);
        fill(&mut second);
        assert_ne!(first, second);
    }

    #[test]
    fn a_fill_actually_changes_the_buffer() {
        let mut bytes = [0u8; 64];
        fill(&mut bytes);
        assert!(bytes.iter().any(|&b| b != 0));
    }

    #[test]
    fn a_stream_keeps_going_across_calls() {
        // Two halves read as one stream must not repeat either: this is what a
        // caller reading `/dev/urandom` in chunks depends on.
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        fill(&mut a);
        fill(&mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn a_reseed_re_keys_the_generator() {
        // `RNDRESEEDCRNG` has to leave the generator in a state no earlier
        // output predicts. The fingerprint reads from a *clone*, so it does not
        // advance the stream itself, and two fingerprints taken without a
        // reseed between them are equal - which is what makes this a
        // counterfactual for the reseed rather than for the passing of time.
        let before = fingerprint();
        assert_eq!(
            before,
            fingerprint(),
            "a passive read must not advance the key"
        );
        reseed();
        let after = fingerprint();
        assert_ne!(before, after, "a reseed must change the key");
        // And a second reseed from the new state changes it again: the re-key
        // is not a reset to a fixed point.
        reseed();
        assert_ne!(after, fingerprint());
    }

    #[test]
    fn a_rekeyed_generator_still_produces_bytes() {
        reseed();
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        fill(&mut a);
        fill(&mut b);
        assert_ne!(a, b);
        assert!(a.iter().chain(b.iter()).any(|&byte| byte != 0));
    }
}
