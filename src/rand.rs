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

/// One ChaCha20 stream, as a value.
///
/// The kernel keeps exactly one of these behind [`RNG`]; the host tests make
/// their own with [`Generator::from_seed`]. That split is what makes the re-key
/// a property of *the generator* rather than of the test runner: a test that
/// draws from the one shared stream can have a sibling test re-key it between
/// two looks, so "a passive read must not advance the key" becomes false for a
/// reason that has nothing to do with this code. The flake and the fix it
/// recorded as the way out: `docs/notes/` §12.4a.
struct Generator(ChaCha20Rng);

impl Generator {
    fn from_seed(seed: [u8; 32]) -> Self {
        Self(ChaCha20Rng::from_seed(seed))
    }

    fn fill(&mut self, out: &mut [u8]) {
        self.0.fill_bytes(out);
    }

    /// Re-key from fresh seed material mixed with a draw from the current
    /// stream; the argument for mixing is on [`reseed`].
    fn reseed(&mut self) {
        let mut carried = [0u8; 32];
        self.0.fill_bytes(&mut carried);
        let mut fresh = seed();
        for (byte, old) in fresh.iter_mut().zip(carried) {
            *byte ^= old;
        }
        self.0 = ChaCha20Rng::from_seed(fresh);
    }

    /// A read of the stream's *future* output that does not consume it: taken
    /// from a clone, so two calls with no `reseed` between them return the same
    /// bytes. Test-only: no caller in the kernel has a use for it.
    #[cfg(all(test, not(target_os = "none")))]
    fn fingerprint(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        self.0.clone().fill_bytes(&mut out);
        out
    }
}

/// The generator, behind the usual kernel lock. `None` until the first use:
/// seeding touches the CPU and the clock, and neither is up as early as this
/// module is compiled.
static RNG: Mutex<RefCell<Option<Generator>>> = Mutex::new(RefCell::new(None));

/// Seed the generator. Idempotent, so a caller may ask at any point in the
/// boot without having to know whether it is the first to arrive.
pub fn init() {
    critical_section::with(|cs| {
        let mut slot = RNG.borrow(cs).borrow_mut();
        if slot.is_none() {
            *slot = Some(Generator::from_seed(seed()));
        }
    });
}

/// A machine's rng device, as something to call: `None` when this machine has
/// none, or has not been wired yet.
type DeviceEntropy = fn() -> Option<[u8; 32]>;

/// The machine's rng device, registered by the wiring layer.
///
/// *Injected* rather than named, for the same reason `gpu::sync::Waker` and
/// `evdev`'s waiter numbers are: the alternative is this module - a kernel
/// service - naming `crate::drivers`, which closes a cycle with the edge that
/// already runs the other way (`drivers::devices` fills `/dev/urandom` from
/// here). `cargo xtask depgraph` counts that as a new mutual pair, and it is
/// right to: which device a machine has is the wiring layer's fact, and
/// `main/<arch>.rs` is where it is said (`NOTES.md` §14.3).
static DEVICE_SOURCE: Mutex<RefCell<Option<DeviceEntropy>>> = Mutex::new(RefCell::new(None));

/// Register the machine's rng device. Called once, from `main/<arch>.rs`,
/// before anything can draw from this generator.
pub fn set_device_entropy(source: DeviceEntropy) {
    critical_section::with(|cs| *DEVICE_SOURCE.borrow(cs).borrow_mut() = Some(source));
}

/// Fill `out` with random bytes.
///
/// Seeds on first use if [`init`] was never called: a syscall or a device read
/// should not have to know the boot order to get entropy.
pub fn fill(out: &mut [u8]) {
    critical_section::with(|cs| {
        let mut slot = RNG.borrow(cs).borrow_mut();
        let rng = slot.get_or_insert_with(|| Generator::from_seed(seed()));
        rng.fill(out);
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
        // `None` (never seeded) contributes nothing and the fresh seed stands
        // alone, which is the same state `init` would have made.
        match slot.as_mut() {
            Some(rng) => rng.reseed(),
            None => *slot = Some(Generator::from_seed(seed())),
        }
    });
}

/// A fresh seed: what the machine can be asked for, mixed with what nobody can
/// guess about it.
///
/// The machine's own sources come first, through `crate::hal` - `RDRAND` and the
/// PCI virtio-rng on x86_64, the MMIO virtio-rng on aarch64, nothing at all on a
/// machine that has neither - and the boot instant is mixed on top of them in
/// every case. Which sources exist is the machine's to know; this function names
/// no architecture and no driver.
///
/// What the clock mix buys is that a machine whose sources are absent or weak
/// still gets a seed an outside caller cannot predict, because when this kernel
/// started is not something a process on it can learn: the clock is under this
/// kernel's control, and reads of it return as much as the kernel chooses to
/// say.
///
/// What this does not do is hold up boot waiting for the entropy pool to fill.
/// A machine that boots headless has no other events to gather from, and an
/// HTTPS client that waits forever for a better seed is worse than one that
/// starts - the seed here is not ideal, and it is not the same twice.
#[cfg(target_os = "none")]
fn seed() -> [u8; 32] {
    let mut seed = [0u8; 32];

    crate::hal::mix_entropy(&mut seed);

    // The rng *device*, on a machine that has one, through the source the
    // wiring layer registered. Deliberately not part of `hal::Entropy`: that
    // trait is the machine's own sources - the CPU's `RDRAND` - while a
    // virtio-rng is a driver the kernel enumerates, so folding it in here is
    // what keeps the machine layer from having to name a driver
    // (`NOTES.md` §14.3). Taken out of the lock before it is called: reading the
    // device is not this module's business and must not happen inside a
    // critical section it holds.
    let source = critical_section::with(|cs| *DEVICE_SOURCE.borrow(cs).borrow());
    if let Some(source) = source {
        if let Some(device) = source() {
            for (byte, host) in seed.iter_mut().zip(device) {
                *byte ^= host;
            }
        }
    }

    // The boot instant and the tick counter, mixed one byte at a time so a
    // zero-filled stretch above is not left as zeroes.
    let epoch = crate::hal::boot_epoch();
    let ticks = crate::hal::ticks();
    let stamp = epoch ^ ticks.rotate_left(32);
    for (i, byte) in seed.iter_mut().enumerate() {
        let word = if i % 2 == 0 { epoch } else { ticks ^ stamp };
        *byte ^= (word.rotate_left((i as u32 % 8) * 8) & 0xff) as u8;
    }
    seed
}

/// The seed for a build that is not the shipped kernel - the host test build.
/// Deterministic on purpose: a unit test that seeds differently every run is a
/// unit test that fails every other time.
#[cfg(not(target_os = "none"))]
fn seed() -> [u8; 32] {
    *b"mpos-host-seed: not a secret, te"
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::Generator;

    /// Each test draws from its own generator: the properties below are about
    /// the generator, and a test that shared the kernel's one stream would fail
    /// whenever a sibling test re-keyed it between two looks - which is the
    /// flake this module used to carry (`docs/notes/` §12.4a), not a property
    /// of the code under test.
    fn generator() -> Generator {
        Generator::from_seed(super::seed())
    }

    #[test]
    fn two_fills_do_not_repeat() {
        let mut rng = generator();
        // The bug this module exists to fix: the old source re-seeded from the
        // tick on every call, so two calls in one tick returned the same bytes.
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        rng.fill(&mut first);
        rng.fill(&mut second);
        assert_ne!(first, second);
    }

    #[test]
    fn a_fill_actually_changes_the_buffer() {
        let mut rng = generator();
        let mut bytes = [0u8; 64];
        rng.fill(&mut bytes);
        assert!(bytes.iter().any(|&b| b != 0));
    }

    #[test]
    fn a_stream_keeps_going_across_calls() {
        let mut rng = generator();
        // Two halves read as one stream must not repeat either: this is what a
        // caller reading `/dev/urandom` in chunks depends on.
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        rng.fill(&mut a);
        rng.fill(&mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn a_reseed_re_keys_the_generator() {
        let mut rng = generator();
        // `RNDRESEEDCRNG` has to leave the generator in a state no earlier
        // output predicts. The fingerprint reads from a *clone*, so it does not
        // advance the stream itself, and two fingerprints taken without a
        // reseed between them are equal - which is what makes this a
        // counterfactual for the reseed rather than for the passing of time.
        let before = rng.fingerprint();
        assert_eq!(
            before,
            rng.fingerprint(),
            "a passive read must not advance the key"
        );
        rng.reseed();
        let after = rng.fingerprint();
        assert_ne!(before, after, "a reseed must change the key");
        // And a second reseed from the new state changes it again: the re-key
        // is not a reset to a fixed point.
        rng.reseed();
        assert_ne!(after, rng.fingerprint());
    }

    #[test]
    fn a_rekeyed_generator_still_produces_bytes() {
        let mut rng = generator();
        rng.reseed();
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        rng.fill(&mut a);
        rng.fill(&mut b);
        assert_ne!(a, b);
        assert!(a.iter().chain(b.iter()).any(|&byte| byte != 0));
    }
}
