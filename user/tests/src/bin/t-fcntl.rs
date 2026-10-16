//! t-fcntl - one test for fcntl, which Rust std's startup uses on fds 0-2
//! (`fcntl(0, F_GETFD) = 0` and friends in the strace).
//!
//! Pinned here: F_GETFD on a live descriptor answers the descriptor's flag
//! word (fresh: 0), F_SETFD stores FD_CLOEXEC and F_GETFD reads it back and
//! clears it again, F_GETFL reports the access mode the description was made
//! under beside the status flags it carries, a command the kernel has no
//! meaning for answers EINVAL, and a descriptor that does not exist answers
//! EBADF. A stub that answers 0 to everything fails the roundtrip by design -
//! "accept and forget" is not flag storage.
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
//! # The `F_GETFL` word is the description's, access mode included
//!
//! The no-std version of this program pinned `F_GETFL` as `O_RDONLY` (0) for
//! every descriptor, on the theory that "opens here are untyped". That was a
//! claim about a kernel keeping the mode in a field of its own - and the mode
//! is exactly the half of the word Linux's `F_SETFL` may *not* change, so the
//! claim could not have caught the divergence it was standing on. What it took
//! was a measurement, which this program now carries
//! (`.workbuddy/probe_getfl.c`, on a current host, every call through
//! `syscall(2)` so no libc's rewrite is mistaken for the kernel's rule):
//!
//! ```text
//! open(O_RDONLY)                          F_GETFL = 0x8000   accmode 0
//! open(O_WRONLY)                          F_GETFL = 0x8001   accmode 1
//! open(O_RDWR)                            F_GETFL = 0x8002   accmode 2
//! open(O_WRONLY|O_APPEND)                 F_GETFL = 0x8401   accmode 1, O_APPEND
//! F_SETFL(O_NONBLOCK) on an O_WRONLY fd   F_GETFL = 0x8801   still accmode 1
//! F_SETFL(O_RDWR)     on an O_WRONLY fd   F_GETFL = 0x8001   still accmode 1
//! F_SETFL(O_RDONLY)   on an O_WRONLY fd   F_GETFL = 0x8001   still accmode 1
//! pipe()      read end 0 (0x0), write end 1 (0x1)
//! socketpair() end 2 (0x2);  eventfd/timerfd/signalfd/epoll 2;  inotify 0
//! ```
//!
//! Linux reports `filp->f_flags`, which is *both* halves: the mode the caller
//! opened with, and the status bits the description kept. `F_SETFL` rewrites it
//! as `(arg & SETFL_MASK) | (old & ~SETFL_MASK)`, and `SETFL_MASK` has no member
//! for the mode - so a caller that asks for `O_NONBLOCK` alone keeps its
//! direction, and one that asks for `O_RDWR` does not get to read.
//!
//! (`0x8000` in each line is `O_LARGEFILE`, which that kernel adds to every
//! path open and which this one does not carry at all; the rest of what Linux's
//! word holds and this kernel's does not - `O_DIRECTORY`, `O_NOFOLLOW`,
//! `O_PATH`, `O_SYNC`, `O_DSYNC`, `O_ASYNC`, `O_DIRECT`, `O_NOATIME` - is
//! named in `docs/uapi/` §5.2.)
//!
//! Every descriptor these claims are made about is one this program *made* -
//! a file it opens, a pipe it mints, a socketpair it makes - so the section
//! measures the same thing under `MPOS_ON_HOST=1` as it does in a payload
//! sweep. The inherited standard descriptors, whose direction is the runner's
//! fixture rather than an answer of this program's, carry no `F_GETFL` claim:
//! the `F_GETFD` ones above are about the descriptor table, which is theirs.
//!
//! # Why two of the claims go through `libc`
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
use rustix::fs::{CWD, Mode, OFlags, fcntl_getfl, fcntl_setfl, openat};
use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::pipe::{PipeFlags, pipe_with};

/// A command number no kernel names, for the `EINVAL` claim. It is written down
/// rather than taken from a table because being outside every table *is* the
/// claim.
const NOT_A_COMMAND: i32 = 999;

/// The file this program owns, for the claims that are about a descriptor a
/// caller opened itself. Its own name rather than a neighbour's, so a leftover
/// from an interrupted run is removed before the fixture is made.
const FILE: &str = "/tmp/t-fcntl_file";

/// `FD_CLOEXEC` with every other bit of the word set too: the word Linux masks,
/// and the word this kernel used to refuse.
const BEYOND_CLOEXEC: u32 = 0xFFFF_FFFF;

/// The `O_ACCMODE` field of an `F_GETFL` word - the two bits that say which way
/// a descriptor opens, and the half of the word every one of these claims is
/// about. The word is read field by field rather than compared whole, because
/// Linux's carries more than the mode: it adds `O_LARGEFILE` to every *path*
/// open (`0x8000` in the probe's transcript), and keeps whatever else the open
/// recorded (`O_DIRECTORY`, `O_NOFOLLOW`, `O_PATH`, `O_SYNC`...). This kernel
/// carries none of those - `docs/uapi/` §5.2 names them - and a pipe end, a
/// socket and a `pipe2` word have no extra bits on either, which is why those
/// three claims below are stated whole.
const ACCMODE: OFlags = OFlags::from_bits_retain(0o3);

/// The access mode of a descriptor, or `None` if the call itself was refused.
fn mode_of(fd: impl std::os::fd::AsFd) -> Option<OFlags> {
    fcntl_getfl(fd).ok().map(|f| f & ACCMODE)
}

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

    // ---- the F_GETFL word: the mode the caller opened with, and the status
    // ---- bits beside it. Every descriptor below is one this program made,
    // ---- so the section means the same thing under `MPOS_ON_HOST=1`.

    let _ = std::fs::remove_file(FILE);
    let Ok(rd) = openat(
        CWD,
        FILE,
        OFlags::CREATE | OFlags::RDONLY,
        Mode::from(0o644),
    ) else {
        r.check("F_GETFL reports O_RDONLY for a file opened O_RDONLY", false);
        r.finish()
    };
    r.check(
        "F_GETFL reports O_RDONLY for a file opened O_RDONLY",
        mode_of(&rd) == Some(OFlags::RDONLY),
    );
    // The creation flags are spent by the open: Linux's word does not report
    // them, and that is the half a word-wide store would leak.
    r.check(
        "F_GETFL does not report O_CREAT",
        fcntl_getfl(&rd).is_ok_and(|f| !f.contains(OFlags::CREATE)),
    );

    let Ok(wr) = openat(CWD, FILE, OFlags::WRONLY, Mode::empty()) else {
        r.check("F_GETFL reports O_WRONLY for a file opened O_WRONLY", false);
        r.finish()
    };
    r.check(
        "F_GETFL reports O_WRONLY for a file opened O_WRONLY",
        mode_of(&wr) == Some(OFlags::WRONLY),
    );
    let Ok(rw) = openat(CWD, FILE, OFlags::RDWR, Mode::empty()) else {
        r.check("F_GETFL reports O_RDWR for a file opened O_RDWR", false);
        r.finish()
    };
    r.check(
        "F_GETFL reports O_RDWR for a file opened O_RDWR",
        mode_of(&rw) == Some(OFlags::RDWR),
    );

    // `F_SETFL` replaces only the bits Linux lets a caller change, so the mode
    // survives every word that can be passed - and no word can hand the
    // description a direction it was not opened with.
    r.check(
        "F_SETFL(O_NONBLOCK) on an O_WRONLY fd succeeds",
        fcntl_setfl(&wr, OFlags::NONBLOCK).is_ok(),
    );
    r.check(
        "and the mode survives it: O_WRONLY, with O_NONBLOCK set",
        fcntl_getfl(&wr)
            .is_ok_and(|f| f & ACCMODE == OFlags::WRONLY && f.contains(OFlags::NONBLOCK)),
    );
    r.check(
        "F_SETFL(empty) clears O_NONBLOCK and leaves the mode",
        fcntl_setfl(&wr, OFlags::empty()).is_ok()
            && fcntl_getfl(&wr)
                .is_ok_and(|f| f & ACCMODE == OFlags::WRONLY && !f.contains(OFlags::NONBLOCK)),
    );
    r.check(
        "F_SETFL(O_RDWR) does not make a write-only fd readable",
        fcntl_setfl(&wr, OFlags::RDWR).is_ok() && mode_of(&wr) == Some(OFlags::WRONLY),
    );
    r.check(
        "nor does F_SETFL(O_RDONLY) make it read-only",
        fcntl_setfl(&wr, OFlags::RDONLY).is_ok() && mode_of(&wr) == Some(OFlags::WRONLY),
    );

    // A pipe's two ends are the one description whose direction the kernel
    // mints rather than the caller's open, and the two ends answer differently.
    match pipe_with(PipeFlags::empty()) {
        Ok((a, b)) => {
            r.check(
                "a pipe's read end reports O_RDONLY",
                fcntl_getfl(&a) == Ok(OFlags::RDONLY),
            );
            r.check(
                "a pipe's write end reports O_WRONLY",
                fcntl_getfl(&b) == Ok(OFlags::WRONLY),
            );
        }
        Err(_) => {
            r.check("a pipe's read end reports O_RDONLY", false);
            r.check("a pipe's write end reports O_WRONLY", false);
        }
    }
    match pipe_with(PipeFlags::NONBLOCK) {
        Ok((a, b)) => r.check(
            "pipe2(O_NONBLOCK)'s ends carry the flag beside the mode",
            fcntl_getfl(&a) == Ok(OFlags::NONBLOCK)
                && fcntl_getfl(&b) == Ok(OFlags::WRONLY | OFlags::NONBLOCK),
        ),
        Err(_) => r.check(
            "pipe2(O_NONBLOCK)'s ends carry the flag beside the mode",
            false,
        ),
    }

    // A socket is the other shape: the kernel mints it read-write, and the
    // answer belongs to the description rather than to the object behind it.
    match socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    ) {
        Ok((a, _b)) => r.check(
            "a socketpair end reports O_RDWR",
            fcntl_getfl(&a) == Ok(OFlags::RDWR),
        ),
        Err(_) => r.check("a socketpair end reports O_RDWR", false),
    }

    let _ = std::fs::remove_file(FILE);

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
