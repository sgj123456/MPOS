//! t-fcntl - one test for fcntl, which Rust std's startup uses on fds 0-2
//! (`fcntl(0, F_GETFD) = 0` and friends in the strace).
//!
//! Pinned here: F_GETFD on a live descriptor answers the descriptor's flag
//! word (fresh: 0), F_SETFD stores FD_CLOEXEC and F_GETFD reads it back and
//! clears it again, F_GETFL answers O_RDONLY (opens here are untyped, so 0),
//! a command the kernel has no meaning for answers EINVAL, and a descriptor
//! that does not exist answers EBADF. A stub that answers 0 to everything
//! fails the roundtrip by design - "accept and forget" is not flag storage.
//!
//! # The word with bits beyond FD_CLOEXEC is *masked*, and that is a fix
//!
//! The no-std version of this program pinned `F_SETFD(fd, 0xFFFF_FFFF)` as
//! `EINVAL` - "argument refused", the shape `AGENTS.md` §3 rule 2 lists. That
//! is not Linux, and the no-std claim had taken its answer from this kernel
//! rather than from the ABI. Measured on a current host through the **raw**
//! syscall, so no libc's masking can be mistaken for the kernel's:
//!
//! ```text
//! fcntl(1, F_SETFD, 0xFFFFFFFF) = 0    then fcntl(1, F_GETFD) = 1
//! fcntl(1, F_SETFD, 0xFFFFFFFE) = 0    then fcntl(1, F_GETFD) = 0
//! ```
//!
//! Linux's `set_close_on_exec(fd, arg & FD_CLOEXEC)` masks the word to the one
//! bit it names and answers success; it refuses nothing. The two claims below
//! were moved onto that answer and the kernel's `F_SETFD` arm with them
//! (migration batch 11): a caller that builds the word from a variable - or
//! reuses one across calls - takes Linux's path here and this kernel's refusal
//! was a shape it cannot recover from the way the ABI's answer lets it.
//!
//! # The one claim here that real Linux would fail, and why it is kept
//!
//! It is the `F_GETFL` one, and it is this kernel's shape rather than the
//! ABI's: measured on a current host, a descriptor opened `O_WRONLY` reports
//! `1` (`O_WRONLY`) from `fcntl(F_GETFL)`, because Linux ORs the descriptor's
//! access mode into the word it reports. This kernel's opens are untyped - its
//! `F_GETFL` arm reports the description's stored status word, whose access
//! bits are always zero, while `src/vfs/file.rs` keeps the mode separately for
//! the read/write `EBADF` refusal and says so beside its `ACC_*` constants. The
//! claim is kept as the no-std version wrote it, because it is a true statement
//! about this kernel and the migration's pin is that *both machines answer it
//! the same*; putting the access mode into the word is a design change with its
//! own blast radius (`rustix::fs::Dir` reuses an `F_GETFL` word for the
//! `openat(fd, ".")` it takes a directory's descriptor with), so
//! `docs/NOTES.md` §15.16 names it with the measurement instead of this program
//! pinning either reading.
//!
//! # Why two of the ten claims go through `libc`
//!
//! Both are the rule's clause (c), the shape that cannot express the claim, and
//! both come from the same fact: `rustix` makes `fcntl` type-safe by splitting
//! it into one function per *command*, so a command the kernel has no meaning
//! for has no spelling in it at all. The descriptor `-1` is the same shape one
//! step down - `rustix` takes `AsFd`, and `BorrowedFd::borrow_raw` refuses a
//! negative raw fd by contract - and `t-write`'s EBADF probe records the same
//! finding. Both go through the *named* wrapper, `libc::fcntl`, rather than
//! `libc::syscall`.
//!
//! The mask claim above does **not** need `libc` even though it is about a word
//! nothing names: `FdFlags` is a bitflags type carrying bitflags' unnamed flag
//! (`const _ = !0`), so `FdFlags::from_bits_retain` is a legal call that hands
//! the kernel the whole word. A rustix *plain enum* has no such escape; that
//! distinction, not "a bitset cannot hold a bit", is what the rule says.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::fs::{OFlags, fcntl_getfl};
use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};

/// A command number no kernel names, for the `EINVAL` claim. It is written down
/// rather than taken from a table because being outside every table *is* the
/// claim.
const NOT_A_COMMAND: i32 = 999;

/// `FD_CLOEXEC` with every other bit of the word set too: the word Linux masks,
/// and the word this kernel used to refuse.
const BEYOND_CLOEXEC: u32 = 0xFFFF_FFFF;

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `fcntl(fd, cmd, arg)` through the named C wrapper, which is where the two
/// claims `rustix` cannot spell have to go. The third argument is variadic and
/// `musl` reads it as an `unsigned long`.
fn fcntl(fd: i32, cmd: i32, arg: libc::c_ulong) -> Result<isize, Errno> {
    let r = unsafe { libc::fcntl(fd, cmd, arg) };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok(r as isize)
    }
}

fn main() {
    println!("t-fcntl");
    let mut r = Report::new();

    // fd 1 is the descriptor Rust std's own startup probes and the one payloads
    // agree on: the runner hands every program the three standard descriptors.
    let stdout = unsafe { BorrowedFd::borrow_raw(1) };

    r.check(
        "F_GETFD on stdout answers a clear flag word",
        fcntl_getfd(stdout) == Ok(FdFlags::empty()),
    );

    r.check(
        "F_SETFD stores FD_CLOEXEC",
        fcntl_setfd(stdout, FdFlags::CLOEXEC) == Ok(()),
    );
    r.check(
        "F_GETFD reads the stored flag back",
        fcntl_getfd(stdout) == Ok(FdFlags::CLOEXEC),
    );

    // The word is taken and masked to the one bit it names - not refused, and
    // not stored whole. See the module comment for the measurement, and for why
    // this pair of claims reads differently from the no-std version's.
    r.check(
        "F_SETFD with bits beyond FD_CLOEXEC succeeds",
        fcntl_setfd(stdout, FdFlags::from_bits_retain(BEYOND_CLOEXEC)) == Ok(()),
    );
    r.check(
        "the stored word is the mask, not the word passed (1, not 0xFFFF_FFFF)",
        fcntl_getfd(stdout) == Ok(FdFlags::CLOEXEC),
    );

    r.check(
        "F_SETFD clears the flag again",
        fcntl_setfd(stdout, FdFlags::empty()) == Ok(()),
    );
    r.check(
        "F_GETFD confirms the cleared flag word",
        fcntl_getfd(stdout) == Ok(FdFlags::empty()),
    );

    r.check(
        "F_GETFL answers O_RDONLY (0) for an untyped open",
        fcntl_getfl(stdout) == Ok(OFlags::empty()),
    );

    r.check(
        "an unknown command answers EINVAL",
        fcntl(1, NOT_A_COMMAND, 0) == Err(Errno::INVAL),
    );
    r.check(
        "a descriptor that does not exist answers EBADF",
        fcntl(-1, libc::F_GETFD, 0) == Err(Errno::BADF),
    );

    r.finish()
}
