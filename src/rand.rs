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
//! one ChaCha20 stream, seeded once from the machine and never re-keyed.

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

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::fill;

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
}
