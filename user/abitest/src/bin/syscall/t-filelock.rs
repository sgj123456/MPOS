//! t-filelock - who a lock belongs to, which is the whole difference between
//! `fcntl` and `flock`.
//!
//! The two families are independent by design (a `flock` never blocks an
//! `fcntl` request and the reverse), but the interesting difference is not that
//! - it is the *owner*. A POSIX record lock belongs to the thread group, so one
//! process locking a range it already holds replaces its own lock instead of
//! deadlocking against itself, and closing **any** descriptor to the file drops
//! the process's record locks (the rule that surprises people). A `flock`
//! belongs to the open file description, so two independent `open()`s of one
//! file inside one process contend with each other, while a `dup`ed descriptor
//! shares the lock its original was given.
//!
//! A kernel that keys both by pid cannot tell those apart, and the difference is
//! visible to an ordinary program: `apk` takes a `flock` on its database, and a
//! second descriptor it opens for the same file must not be handed a lock it
//! already appears to hold - and must not have it dropped under it when an
//! unrelated descriptor closes.
//!
//! The `struct flock` is laid out by hand (x86_64: `l_type`/`l_whence` at 0/2,
//! `l_start` at 8, `l_len` at 16, `l_pid` at 24, 32 bytes) because the point is
//! the ABI, not a libc's convenience.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: &[u8] = b"/tmp/t-filelock.bin\0";

const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const F_DUPFD: i32 = 0;
const F_GETLK: i32 = 5;
const F_SETLK: i32 = 6;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
const SEEK_SET: i16 = 0;

const LOCK_SH: usize = 1;
const LOCK_EX: usize = 2;
const LOCK_NB: usize = 4;
const LOCK_UN: usize = 8;

const EWOULDBLOCK: isize = -11;

/// A `struct flock`.
struct Flock([u8; 32]);

impl Flock {
    /// A write lock over `[start, start+len)`; `len` of 0 is "to the end".
    fn writer(start: i64, len: i64) -> Flock {
        let mut f = Flock([0u8; 32]);
        f.set_i16(0, F_WRLCK);
        f.set_i16(2, SEEK_SET);
        f.set_i64(8, start);
        f.set_i64(16, len);
        f
    }

    fn set_i16(&mut self, at: usize, v: i16) {
        self.0[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn set_i64(&mut self, at: usize, v: i64) {
        self.0[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn ptr(&mut self) -> usize {
        self.0.as_mut_ptr() as usize
    }

    /// `l_type`, which is how `F_GETLK` answers: the kind of the lock in the
    /// way, or `F_UNLCK` when nothing is.
    fn kind(&self) -> i16 {
        i16::from_le_bytes([self.0[0], self.0[1]])
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(_argc: i32, _argv: *const *const u8) -> ! {
    lin::puts(b"t-filelock\n");
    let mut failed = false;

    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    let first = lin::openat(lin::AT_FDCWD, PATH, O_RDWR | O_CREAT, 0o644);
    lin::check_true(&mut failed, b"the file is created", first >= 0);
    // A second, *independent* description of the same file - the case a
    // per-process lock table cannot see - and a `dup` of the first, which is
    // the *same* description.
    let second = lin::openat(lin::AT_FDCWD, PATH, O_RDWR, 0);
    let dup = lin::fcntl(first as i32, F_DUPFD, 100);
    lin::check_true(
        &mut failed,
        b"the second open() and the dup both exist",
        second >= 0 && dup >= 0,
    );

    // --- flock: owned by the description ---
    lin::check_eq(
        &mut failed,
        b"the first description takes LOCK_EX",
        lin::flock(first as i32, LOCK_EX),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a second open() contends with it, same process and all",
        lin::flock(second as i32, LOCK_EX | LOCK_NB),
        EWOULDBLOCK,
    );
    lin::check_eq(
        &mut failed,
        b"a dup shares the description's lock instead",
        lin::flock(dup as i32, LOCK_EX | LOCK_NB),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the holder releases",
        lin::flock(first as i32, LOCK_UN),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"and the contending description now takes it",
        lin::flock(second as i32, LOCK_EX | LOCK_NB),
        0,
    );

    // --- fcntl: owned by the process ---
    let mut held = Flock::writer(0, 0);
    lin::check_eq(
        &mut failed,
        b"a record lock is taken through the first descriptor",
        lin::fcntl(first as i32, F_SETLK, held.ptr()),
        0,
    );
    let mut again = Flock::writer(0, 0);
    lin::check_eq(
        &mut failed,
        b"another descriptor of the same process replaces it rather than fighting it",
        lin::fcntl(second as i32, F_SETLK, again.ptr()),
        0,
    );
    // With no flock held anywhere, a record lock alone must not refuse a
    // `flock`: the families never see each other.
    lin::check_eq(
        &mut failed,
        b"the second description releases its flock",
        lin::flock(second as i32, LOCK_UN),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a held record lock does not refuse a flock",
        lin::flock(dup as i32, LOCK_SH | LOCK_NB),
        0,
    );
    // Closing a description drops the process's *record* locks wherever they
    // were taken (POSIX's rule) and this description's *flock* with it - which
    // is the pair to check together, because they look like opposites.
    lin::check_eq(
        &mut failed,
        b"closing that unrelated descriptor...",
        lin::close(dup as i32),
        0,
    );
    let mut probe = Flock::writer(0, 0);
    lin::fcntl(first as i32, F_GETLK, probe.ptr());
    lin::check_true(
        &mut failed,
        b"...drops the record lock taken through a different fd...",
        probe.kind() == F_UNLCK,
    );
    lin::check_eq(
        &mut failed,
        b"...and the flock went with the description that held it",
        lin::flock(first as i32, LOCK_EX | LOCK_NB),
        0,
    );

    let _ = lin::flock(first as i32, LOCK_UN);
    let _ = lin::close(first as i32);
    let _ = lin::close(second as i32);
    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-filelock: panicked\n")
}
