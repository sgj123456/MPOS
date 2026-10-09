//! t-tty-eintr - a blocked terminal read is ended by a signal the process has
//! trapped, and is *not* ended by one it has not.
//!
//! This is the rule that made `vim` and busybox `vi` stop responding with the
//! screen drawn and nothing else happening. Their own timeouts are signals -
//! `SIGALRM` from `alarm`/`setitimer`, `SIGWINCH` from a resize - and on Linux a
//! signal that arrives during a blocked `read` runs the handler and then ends
//! the read with `EINTR`, so the program learns its wait is over and looks
//! again. Here the signal woke the task, the read re-tested the empty queue, and
//! the task went back to sleep: the signal was observed and thrown away, and the
//! program never came back. Measured on Linux for `SIGALRM`, `SIGWINCH` and
//! `SIGCHLD` alike with a handler installed (`-1/EINTR`, handler run first), and
//! measured as *not* an interruption, just as load-bearing, for a signal whose
//! default action is to be ignored and which nobody trapped: a `SIGCHLD` from an
//! exiting child, a `SIGWINCH` with no handler, a `SIGURG`, a `SIGCONT` left the
//! read blocked for its full watchdog. That second half is what keeps a shell's
//! read of its own terminal from coming back `EINTR` after every job it runs.
//!
//! Both halves are pinned here, against a pty slave so the read can be given a
//! line at a chosen moment:
//!
//! 1. handler installed, `ITIMER_REAL` due mid-read, no line yet: `EINTR`, with
//!    the handler counted as having run *before* the return.
//! 2. the read retried after that `EINTR` still gets the line that was typed
//!    while it was not reading - the interrupted read consumed nothing.
//! 3. `SIG_IGN` instead of a handler, the same timer due mid-read: the read is
//!    *not* interrupted, and comes back with the line when it lands.
//!
//! `SIGCHLD` arrives throughout, from the writer children: with the third rule
//! unimplemented the second read would answer `EINTR` instead of a line, which
//! is how a full-screen program with a trapped `SIGCHLD` would have been told
//! its terminal had failed on every `wait`.
//!
//! # The doors
//!
//! The install is `libc::sigaction`, whose musl build fills the restorer word
//! itself - `t-sa-restart` is the other half of this program and pins the
//! `SA_RESTART` the same wrapper writes. The interrupted read is
//! `rustix::io::read`: a plain `read`, with no `EINTR` retry in its backend,
//! which is the only way the `EINTR` is observable at all. The timer is
//! `libc::setitimer`, whose `ITIMER_REAL` is not defined for any machine this
//! crate builds for (measured: this libc defines it under fuchsia, hurd, aix
//! and nto and in no `linux_like` file), so it is written here. The fixture's
//! pair is `t-pty`'s: `/dev/ptmx` → `TIOCGPTN` → `TIOCSPTLCK(0)` →
//! `/dev/pts/N`, both ends `O_NOCTTY`, with the two requests taken from
//! `libc`'s own architecture table and driven through
//! `libc::syscall(SYS_ioctl)` (`t-vt`'s reason: `libc::ioctl`'s request
//! parameter is `c_int` on musl and `c_ulong` on gnu).
//!
//! The clock is `std::time::Instant`, and the timing is a fixture rather than a
//! claim: what is claimed is that the wait ended at the signal and not at the
//! line, which is an inequality between two events the test itself placed.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, waitpid};
use std::ffi::CString;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Instant;

/// `ITIMER_REAL`: the wall-clock timer `alarm(2)` is a shortcut for. Named by
/// this libc for fuchsia, hurd, aix and nto and in no `linux_like` file
/// (measured), so it is written here.
const ITIMER_REAL: libc::c_int = 0;

/// How many times the handler ran, and the signal it saw. Atomics because
/// nothing on the main line "touches" the handler, and a plain static would let
/// the compiler keep the read of it in a register.
static RAN: AtomicUsize = AtomicUsize::new(0);
static SEEN: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_alarm(sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    SEEN.store(sig, Ordering::Relaxed);
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// One ioctl, with `errno` read immediately on failure - `t-sa-restart`'s
/// helper, for the reason its comment gives.
fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: usize) -> (libc::c_long, i32) {
    // SAFETY: the pointer-shaped argument is whatever the caller passed, and
    // every caller here passes a buffer it owns.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            fd as libc::c_long,
            request as libc::c_long,
            arg,
        )
    };
    (
        rc,
        if rc < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        },
    )
}

/// A fresh pty pair, its slave's path read back from `TIOCGPTN` rather than
/// written down - `t-sa-restart`'s fixture and `t-pty`'s sequence.
fn fresh_pair() -> (OwnedFd, OwnedFd) {
    let master = open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
        .unwrap_or_else(|e| panic!("opening /dev/ptmx: {e}"));
    let mut number: u32 = 0;
    let (rc, err) = ioctl(
        master.as_fd().as_raw_fd(),
        libc::TIOCGPTN as libc::c_ulong,
        &mut number as *mut u32 as usize,
    );
    if rc != 0 {
        panic!("TIOCGPTN failed: errno {err}");
    }
    let unlocked: i32 = 0;
    let (rc, err) = ioctl(
        master.as_fd().as_raw_fd(),
        libc::TIOCSPTLCK as libc::c_ulong,
        &unlocked as *const i32 as usize,
    );
    if rc != 0 {
        panic!("TIOCSPTLCK failed: errno {err}");
    }
    let path = CString::new(format!("/dev/pts/{number}")).expect("a path with no NUL in it");
    let slave = open(
        path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .unwrap_or_else(|e| panic!("opening {path:?}: {e}"));
    (master, slave)
}

/// Leave through `exit_group`: the child's inherited copy of the parent's
/// stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    // SAFETY: the door does not return; `_exit` is the maintained fallback.
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// Sleep out a whole number of milliseconds; only fixture children and the
/// setup between checks ever call it.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: a `timespec` this call owns, and a null second pointer.
    unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
}

/// Give `SIGALRM` a disposition. `None` is `SIG_IGN`, which is the other half
/// of the rule this program pins.
fn aim_alarm(handler: Option<usize>) -> bool {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    match handler {
        Some(handler) => {
            act.sa_sigaction = handler;
            act.sa_flags = libc::SA_SIGINFO;
        }
        None => {
            act.sa_sigaction = libc::SIG_IGN;
            act.sa_flags = 0;
        }
    }
    // SAFETY: `act` is filled for a number the wrapper serves, and no old
    // record is wanted.
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) == 0 }
}

/// Type `bytes` at the slave - through the master, the emulator's end - after
/// `delay` ms, from a child, so the read being pinned is already parked when
/// the line lands.
fn type_later(master: BorrowedFd<'_>, delay_ms: i64, bytes: &'static [u8]) -> i32 {
    // SAFETY: the child sleeps, writes to a descriptor it owns, and exits; it
    // shares nothing mutable with the parent.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        sleep_ms(delay_ms);
        let _ = rustix::io::write(master, bytes);
        leave(0);
    }
    pid
}

/// One timed read of the slave, reporting the count (or the refusal's number),
/// the elapsed milliseconds and the bytes themselves.
fn timed_read(fd: BorrowedFd<'_>) -> (Result<usize, i32>, u64, [u8; 16]) {
    let mut buf = [0u8; 16];
    let at = Instant::now();
    let got = rustix::io::read(fd, &mut buf).map_err(Errno::raw_os_error);
    let ms = at.elapsed().as_millis() as u64;
    (got, ms, buf)
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now: the same route `alarm(2)`
/// takes, and the one a full-screen program's timeout is built on.
fn alarm_once(ms: u64) -> bool {
    // `as _` at the field rather than the field's own alias: this libc
    // deprecates `time_t`/`suseconds_t` for a reason that is not this file's
    // (the batch 17 note on the same warning in `t-poll`).
    let secs = (ms / 1000) as i64;
    let usecs = ((ms % 1000) * 1000) as i64;
    let next = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: secs as _,
            tv_usec: usecs as _,
        },
    };
    let mut spent = next;
    // SAFETY: two `itimerval`s this process owns, and the one clock that
    // measures real time.
    unsafe { libc::setitimer(ITIMER_REAL, &next, &mut spent) == 0 }
}

fn reap(kid: i32) {
    let _ = waitpid(Pid::from_raw(kid), WaitOptions::empty());
}

fn main() {
    println!("t-tty-eintr");
    let mut r = Report::new();

    let (master, slave) = fresh_pair();

    // 1. A line typed at 600 ms, a timer due at 200 ms: the read must come back
    //    `EINTR` at the timer, with the handler already run - not sleep to the
    //    line, and not answer the line as if the signal had never happened.
    r.check(
        "a SIGALRM handler installs",
        aim_alarm(Some(
            on_alarm as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
                as usize,
        )),
    );
    let kid = type_later(master.as_fd(), 600, b"late\r");
    r.check("ITIMER_REAL arms", alarm_once(200));
    let (got, ms, _) = timed_read(slave.as_fd());
    r.check(
        "a trapped signal ends the blocked read with EINTR",
        got == Err(libc::EINTR),
    );
    r.check("the wait ended at the signal, not at the line", ms < 500);
    r.check(
        "the handler ran exactly once before the return",
        RAN.load(Ordering::Relaxed) == 1,
    );
    r.check(
        "and the signal it ran for is SIGALRM",
        SEEN.load(Ordering::Relaxed) == libc::SIGALRM,
    );

    // 2. The retry gets the line. A read that had consumed bytes on its way out
    //    would hand back a truncated one here, and `strace` shows why it must
    //    not: the kernel's own answer for the interrupted read is
    //    `ERESTARTSYS` with nothing transferred.
    let (got, ms, buf) = timed_read(slave.as_fd());
    r.check(
        "the retried read takes the whole line typed mid-interrupt",
        got == Ok(5),
    );
    r.check(
        "and it is that line",
        got == Ok(5) && &buf[..5] == b"late\n",
    );
    r.check("and it waited for it", ms >= 100);
    reap(kid);

    // 3. The same timer with nothing trapped for it: no interruption. This is
    //    the half a naive rule loses - "any pending signal ends the wait" would
    //    break a shell's own read on every job it finishes, and it is not what
    //    Linux does.
    r.check("SIGALRM is set to SIG_IGN", aim_alarm(None));
    let kid = type_later(master.as_fd(), 400, b"hi\r");
    r.check("ITIMER_REAL arms again", alarm_once(150));
    let (got, ms, buf) = timed_read(slave.as_fd());
    r.check(
        "an ignored signal does not interrupt the read",
        got == Ok(3),
    );
    r.check(
        "the line arrives as typed",
        got == Ok(3) && &buf[..3] == b"hi\n",
    );
    r.check(
        "and it was the line, not the signal, that ended the wait",
        ms >= 300,
    );
    r.check("no further handler ran", RAN.load(Ordering::Relaxed) == 1);
    reap(kid);

    // Leave the signal the way it was found: a disposition left on `SIG_IGN`
    // would hide the next program's alarm failures.
    let mut back: libc::sigaction = unsafe { std::mem::zeroed() };
    back.sa_sigaction = libc::SIG_DFL;
    // SAFETY: the default disposition for a number the wrapper serves.
    unsafe { libc::sigaction(libc::SIGALRM, &back, std::ptr::null_mut()) };

    drop(slave);
    drop(master);
    r.finish()
}
