//! t-anon-fd - Linux's anonymous descriptors report a type-less stat, an
//! `anon_inode` name in `/proc/self/fd`, and their own zero-length read.
//!
//! The family is `eventfd`, `timerfd`, `signalfd`, `epoll`, `inotify` and
//! `pidfd`: descriptors that name no file and live in no tree. Linux gives all
//! six the same shape - `st_mode` has **no** `S_IF*` bits - and then parts ways
//! on two details, both measured on a current host first
//! (`.workbuddy/probe_anon_fd_stat.c`, `.workbuddy/probe_anon_read0.c`,
//! `.workbuddy/probe_inotify0*.c`):
//!
//! * the permission bits are `0600` for the five descriptor families and `0700`
//!   for a pidfd;
//! * `/proc/self/fd/<n>` reads back `anon_inode:[eventfd]`, `[timerfd]`,
//!   `[signalfd]`, `[eventpoll]`, `[pidfd]` - and, unlike every other member of
//!   the family, `anon_inode:inotify` with **no** brackets.
//!
//! This kernel used to answer `S_IFCHR` for the five (borrowing the engine
//! driver's type) and the `anon:[<ino>]` fallback for the name, which is what a
//! `/proc` reader saw. The type is the wrong shape for a tool that branches on
//! `S_ISCHR`: it hands the descriptor to a terminal or a device probe it is
//! not. The zero-length read is pinned here too, because it is the same
//! "what is this descriptor" question asked through the read door: three
//! families answer `EINVAL` before looking at their queue, a non-blocking
//! inotify answers `EAGAIN` while empty and `EINVAL` once a record is queued.
//!
//! A regular file is the control: its link still names the path it was opened
//! by, so "every descriptor reads back an anon name" would fail here.

use mpos_user_tests::Report;
use rustix::event::epoll::{CreateFlags as EpollCreateFlags, create as epoll_create};
use rustix::event::{EventfdFlags, eventfd};
use rustix::fs::inotify::{CreateFlags as InotifyCreateFlags, init as inotify_init};
use rustix::fs::{CWD, Mode, OFlags, fstat, openat, readlinkat};
use rustix::io::{Errno, read};
use rustix::process::{PidfdFlags, getpid, pidfd_open};
use rustix::time::{TimerfdClockId, TimerfdFlags, timerfd_create};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

/// What `/proc/self/fd/<n>` answers.
fn link_of(fd: BorrowedFd<'_>) -> String {
    let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
    readlinkat(CWD, path, Vec::new())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|e| format!("<readlink: {e}>"))
}

/// The mode type bits of Linux's `S_IFMT`.
const S_IFMT: u32 = 0o170_000;

/// One family's three claims: the type-less mode, its permission bits, and the
/// `anon_inode` name.
fn check_shape(r: &mut Report, label: &str, fd: BorrowedFd<'_>, name: &str, perms: u32) {
    match fstat(fd) {
        Ok(st) => {
            let mode = st.st_mode as u32;
            println!(
                "     {label}: mode={mode:06o} (type={:06o}) link={}",
                mode & S_IFMT,
                link_of(fd)
            );
            r.check(
                &format!("{label}: st_mode has no file type"),
                mode & S_IFMT == 0,
            );
            r.check(
                &format!("{label}: permission bits are {perms:04o}"),
                mode & 0o777 == perms,
            );
        }
        Err(e) => {
            r.check(&format!("{label}: fstat failed ({e})"), false);
        }
    }
    r.check(
        &format!("{label}: /proc/self/fd names {name}"),
        link_of(fd) == name,
    );
}

/// A zero-length read's answer. Linux checks the count before the queue for
/// these five (measured), so emptiness does not matter.
fn check_read_zero(r: &mut Report, label: &str, fd: BorrowedFd<'_>, want: Errno) {
    let mut empty: [u8; 0] = [];
    match read(fd, &mut empty) {
        Ok(n) => {
            r.check(
                &format!("{label}: zero-length read is {want:?}, not {n}"),
                false,
            );
        }
        Err(e) => r.check(&format!("{label}: zero-length read is {want:?}"), e == want),
    }
}

fn main() {
    let mut r = Report::new();

    // The five `0600` families. Each is non-blocking so the zero-length read
    // below is an answer rather than a wait.
    match eventfd(0, EventfdFlags::NONBLOCK) {
        Ok(fd) => {
            check_shape(&mut r, "eventfd", fd.as_fd(), "anon_inode:[eventfd]", 0o600);
            check_read_zero(&mut r, "eventfd", fd.as_fd(), Errno::INVAL);
        }
        Err(e) => r.check(&format!("eventfd could not be made ({e})"), false),
    }

    match timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::NONBLOCK) {
        Ok(fd) => {
            check_shape(&mut r, "timerfd", fd.as_fd(), "anon_inode:[timerfd]", 0o600);
            check_read_zero(&mut r, "timerfd", fd.as_fd(), Errno::INVAL);
        }
        Err(e) => r.check(&format!("timerfd could not be made ({e})"), false),
    }

    // `signalfd` has no `rustix` binding (measured for this crate: it is not
    // there), so the call is `libc`'s - the same door `t-signalfd` pins.
    let signal_fd = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        libc::signalfd(-1, &set, libc::SFD_NONBLOCK)
    };
    if signal_fd >= 0 {
        // SAFETY: the descriptor is open and owned here.
        let fd = unsafe { BorrowedFd::borrow_raw(signal_fd) };
        check_shape(&mut r, "signalfd", fd, "anon_inode:[signalfd]", 0o600);
        check_read_zero(&mut r, "signalfd", fd, Errno::INVAL);
        // SAFETY: this is the only owner of the number.
        unsafe { libc::close(signal_fd) };
    } else {
        r.check("signalfd could not be made", false);
    }

    match epoll_create(EpollCreateFlags::CLOEXEC) {
        Ok(fd) => {
            check_shape(&mut r, "epoll", fd.as_fd(), "anon_inode:[eventpoll]", 0o600);
            check_read_zero(&mut r, "epoll", fd.as_fd(), Errno::INVAL);
        }
        Err(e) => r.check(&format!("epoll could not be made ({e})"), false),
    }

    match pidfd_open(getpid(), PidfdFlags::empty()) {
        Ok(fd) => {
            check_shape(&mut r, "pidfd", fd.as_fd(), "anon_inode:[pidfd]", 0o700);
            check_read_zero(&mut r, "pidfd", fd.as_fd(), Errno::INVAL);
        }
        Err(e) => r.check(&format!("pidfd could not be made ({e})"), false),
    }

    // inotify is the one whose zero-length read consults the queue first.
    match inotify_init(InotifyCreateFlags::NONBLOCK) {
        Ok(fd) => {
            check_shape(&mut r, "inotify", fd.as_fd(), "anon_inode:inotify", 0o600);
            // Nothing queued: EAGAIN on a non-blocking descriptor.
            check_read_zero(&mut r, "inotify empty", fd.as_fd(), Errno::AGAIN);
            // One record queued: the zero count is EINVAL.
            let _ = rustix::fs::inotify::add_watch(
                &fd,
                "/tmp",
                rustix::fs::inotify::WatchFlags::CREATE,
            );
            let _ = openat(
                CWD,
                "/tmp/t-anon-fd.event",
                OFlags::CREATE | OFlags::WRONLY,
                Mode::from_raw_mode(0o600),
            );
            let _ = std::fs::remove_file("/tmp/t-anon-fd.event");
            check_read_zero(&mut r, "inotify queued", fd.as_fd(), Errno::INVAL);
        }
        Err(e) => r.check(&format!("inotify could not be made ({e})"), false),
    }

    // The control: a descriptor that really was opened by a path reports that
    // path, so an implementation that answered an anon name for everything
    // fails here.
    match openat(
        CWD,
        "/tmp/t-anon-fd.control",
        OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(fd) => {
            r.check(
                "a path-opened file still reports its path",
                link_of(fd.as_fd()) == "/tmp/t-anon-fd.control",
            );
            let _ = std::fs::remove_file("/tmp/t-anon-fd.control");
        }
        Err(e) => r.check(
            &format!("the control file could not be opened ({e})"),
            false,
        ),
    }

    r.finish();
}
