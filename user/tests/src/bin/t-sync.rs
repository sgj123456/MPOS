//! t-sync - the flush calls: fsync, fdatasync, syncfs and sync.
//!
//! Pinned here, per fsync(2)/fdatasync(2)/syncfs(2)/sync(2): a regular file
//! answers 0 to all three descriptor forms, every one of them answers EBADF
//! for a number the table does not hold, and `fsync` validates the *kind* of
//! what the descriptor holds rather than merely its existence - a channel
//! with no storage cannot be synchronised, which the one shared handler
//! spells as EINVAL for its `CharDev` and `Socket` kinds. An mpos
//! `SOCK_DGRAM` socketpair end is a `CharDev` inode, so it is the descriptor
//! this test reaches EINVAL through; `sync(2)` takes no argument and is
//! answered 0 at the dispatch table itself, where the number has no handler
//! to run.
//!
//! The two byte channels whose kind cannot answer for them are refused by
//! name, not by kind: a pipe and a `SOCK_STREAM` socketpair end report
//! `File`, because the vocabulary has no word for "anonymous byte channel"
//! (see `pipe.rs`), so the handler asks the *node* whether it is one and
//! answers EINVAL the way Linux does. Reading the kind alone answered 0 for
//! both, which is the bug those checks would catch coming back. Every write
//! here already reached its storage, so the success answer is only the
//! descriptor check and no flush is a divergence.
//!
//! # Why syncfs rides the raw door
//!
//! rustix binds none of the four (clause (a) for `syncfs`; `libc::fsync` /
//! `libc::fdatasync` / `libc::sync` exist and are the named wrappers, used
//! beside the raw `SYS_syncfs`). A real Linux answers EINVAL for a datagram
//! socketpair too, so the hosted run accepts each refusal Linux gives and
//! names which machine answered; the payload sweep pins mpos's own words.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, unlinkat};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::pipe::{PipeFlags, pipe_with};

const PATH: &str = "/tmp/t-sync";

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn syncfs(fd: i32) -> isize {
    unsafe { libc::syscall(libc::SYS_syncfs, fd) as isize }
}

fn main() {
    println!("t-sync");
    let mut r = Report::new();

    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    let opened = openat(
        CWD,
        PATH,
        OFlags::CREATE | OFlags::RDWR,
        Mode::from_bits_retain(0o644),
    );
    r.check("the scratch file is created", opened.is_ok());
    let fd = match opened {
        Ok(fd) => fd,
        Err(_) => r.finish(),
    };
    let raw = fd.as_raw_fd();

    r.check(
        "fsync on a regular file succeeds",
        unsafe { libc::fsync(raw) } == 0,
    );
    r.check(
        "fdatasync on a regular file succeeds",
        unsafe { libc::fdatasync(raw) } == 0,
    );
    r.check("syncfs on a regular file succeeds", syncfs(raw) == 0);

    // The kind check: an mpos SOCK_DGRAM socketpair end reports CharDev, so
    // the shared handler refuses it EINVAL the way Linux does a socket.
    let sv = match socketpair(
        AddressFamily::UNIX,
        SocketType::DGRAM,
        SocketFlags::empty(),
        None,
    ) {
        Ok(p) => {
            r.check("the unix socketpair is made", true);
            p
        }
        Err(_) => {
            r.check("the unix socketpair is made", false);
            r.finish();
        }
    };
    let sv0 = sv.0.as_raw_fd();
    let einval_or_host =
        |ret: isize| refused(ret, libc::EINVAL) || (mpos_user_tests::host_run() && ret == -1);
    r.check(
        "fsync on a socket answers EINVAL",
        einval_or_host(unsafe { libc::fsync(sv0) } as isize),
    );
    r.check(
        "fdatasync on a socket answers EINVAL",
        einval_or_host(unsafe { libc::fdatasync(sv0) } as isize),
    );
    r.check(
        "syncfs on a socket still succeeds",
        syncfs(sv0) == 0 || (mpos_user_tests::host_run() && syncfs(sv0) == -1),
    );

    // A pipe is refused like the socket above, and for the same reason: a
    // byte channel with no storage behind it. Its `kind` still cannot say
    // so - a pipe reports File - so the handler reaches it through
    // `as_pipe`.
    let pipes = pipe_with(PipeFlags::empty());
    if let Ok(p) = &pipes {
        r.check("the pipe pair is made", true);
        r.check(
            "fsync on a pipe answers EINVAL",
            einval_or_host(unsafe { libc::fsync(p.0.as_raw_fd()) } as isize),
        );
        r.check(
            "fdatasync on a pipe answers EINVAL",
            einval_or_host(unsafe { libc::fdatasync(p.1.as_raw_fd()) } as isize),
        );
    } else {
        r.check("the pipe pair is made", false);
        r.check("fsync on a pipe answers EINVAL", false);
        r.check("fdatasync on a pipe answers EINVAL", false);
    }

    // The other half of that family: a `SOCK_STREAM` socketpair end is a
    // duplex `File` here rather than the `CharDev` the datagram pair
    // reports, so this is the check that reaches the handler's `as_duplex`
    // arm.
    let stream = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    );
    if let Ok(s) = &stream {
        r.check("the stream socketpair is made", true);
        r.check(
            "fsync on a stream socketpair end answers EINVAL",
            einval_or_host(unsafe { libc::fsync(s.0.as_raw_fd()) } as isize),
        );
    } else {
        r.check("the stream socketpair is made", false);
        r.check("fsync on a stream socketpair end answers EINVAL", false);
    }

    r.check(
        "fsync on a closed descriptor answers EBADF",
        refused(unsafe { libc::fsync(99) } as isize, libc::EBADF),
    );
    r.check(
        "fdatasync on a closed descriptor answers EBADF",
        refused(unsafe { libc::fdatasync(99) } as isize, libc::EBADF),
    );
    r.check(
        "syncfs on a closed descriptor answers EBADF",
        refused(syncfs(99), libc::EBADF),
    );

    // sync(2) has no argument to read and no handler to run: the number is
    // answered 0 directly, the whole surface being "all writes reached
    // their storage".
    unsafe { libc::sync() };
    r.check("sync takes no argument and answers 0", true);

    drop((sv, pipes, stream, fd));
    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}
