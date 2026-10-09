//! t-tty-nonblock - `O_NONBLOCK` on a terminal: what a read answers when it was
//! told not to wait, and which end of a pty pair the bit belongs to.
//!
//! `t-tty-vtime` pins the *blocking* `VMIN`/`VTIME` table. This is the same
//! table with the one flag that removes the waiting, and every row is measured
//! on Linux (2026-10-01, a pty slave, raw mode):
//!
//! | mode | queued | answered |
//! |---|---|---|
//! | `0`/`0` | nothing | 0 bytes - the poll, which never waited |
//! | `1`/`0` | nothing | `EAGAIN` |
//! | `0`/`5` | nothing | `EAGAIN` at 0 ms: **the clock is not consulted** |
//! | `2`/`5` | 1 byte | that byte, immediately: the minimum is a floor on a *wait* |
//! | `1`/`0` | 1 byte | that byte |
//! | canonical | a partial line | `EAGAIN` |
//! | canonical | a complete line | the line |
//!
//! Before this the kernel consulted the flag nowhere on a terminal, so the
//! `1`/`0` row parked for ever and the `0`/`5` row slept its half second: a
//! reader with an event loop - a supervisor driving a pair, the console
//! multiplexer's clients - froze on the first empty read instead of hearing
//! `EAGAIN`.
//!
//! The second half is the plumbing, because the flag lives on the *open file
//! description* and the blocking lives on the node: `open(O_NONBLOCK)` and
//! `fcntl(F_SETFL, O_NONBLOCK)` have to reach it, and a pty is one terminal
//! with two ends whose flags are their own - the master's must not decide the
//! slave's read, even though `PtyMaster` answers the slave's terminal for every
//! termios question.
//!
//! # The doors
//!
//! The install is `libc::sigaction`, the same wrapper `t-tty-eintr` uses, and
//! the guard is `ITIMER_REAL` through `libc::setitimer` - this libc names
//! `ITIMER_REAL` for fuchsia, hurd, aix and nto and in no `linux_like` file
//! (measured), so it is written here. The pair is `t-pty`'s sequence, the
//! requests taken from `libc`'s architecture table and driven through
//! `libc::syscall(SYS_ioctl)`; the mode is `rustix::termios`, which is the only
//! table that names `VMIN`/`VTIME`/`ICANON` for Linux (see `t-tty-vtime`). The
//! flag's own two routes are `OFlags::NONBLOCK` at `open` and
//! `rustix::fs::fcntl_getfl`/`fcntl_setfl` - `F_GETFL`/`F_SETFL` with
//! `O_NONBLOCK` in an `OFlags`, which is what the flag is. (That `F_GETFL`
//! reports no access mode on this kernel is `t-fcntl`'s divergence, not this
//! program's: the mask it feeds back is this end's own.)
//!
//! The clock is `std::time::Instant`, and the guard on every read that a
//! blocking terminal would park through is deliberate: without the fix the read
//! answers `EINTR` (or never returns at all), and a hang here would look like a
//! kernel crash rather than a claim.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
use std::ffi::CString;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Instant;

/// `ITIMER_REAL`: the wall-clock timer `alarm(2)` is a shortcut for. Named by
/// this libc for fuchsia, hurd, aix and nto and in no `linux_like` file
/// (measured), so it is written here.
const ITIMER_REAL: libc::c_int = 0;

/// How many times the alarm handler ran: every one of them is an interrupted
/// wait, which is what a *blocking* read of an empty queue is.
static RAN: AtomicUsize = AtomicUsize::new(0);
static SEEN: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_alarm(sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    SEEN.store(sig, Ordering::Relaxed);
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// A handler with no `SA_RESTART`, so an interrupted read comes back `EINTR` -
/// the guard that turns "this call parked" into a failing claim instead of a
/// test that hangs.
fn install_alarm() -> bool {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        on_alarm as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves, and
    // no old record wanted.
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) == 0 }
}

/// Arm `ITIMER_REAL` once, `ms` from now, or disarm it with `None`.
fn alarm(ms: Option<u64>) {
    // `as _` at the field rather than the field's own alias: this libc
    // deprecates `time_t`/`suseconds_t` for a reason that is not this file's.
    let secs = (ms.unwrap_or(0) / 1000) as i64;
    let usecs = ((ms.unwrap_or(0) % 1000) * 1000) as i64;
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
    unsafe { libc::setitimer(ITIMER_REAL, &next, &mut spent) };
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

/// A fresh pty pair. The slave is opened with the flags asked for, which is one
/// of the two routes the bit can take.
fn fresh_pair(slave_flags: OFlags) -> (OwnedFd, OwnedFd) {
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
        OFlags::RDWR | OFlags::NOCTTY | slave_flags,
        Mode::empty(),
    )
    .unwrap_or_else(|e| panic!("opening {path:?}: {e}"));
    (master, slave)
}

/// Put the terminal in the mode a full-screen program puts it in - no line
/// editing, no echo - with the `VMIN`/`VTIME` pair under test. Reports whether
/// the kernel took the settings, because a test that silently ran in another
/// mode would prove nothing.
fn set_mode(slave: BorrowedFd<'_>, vmin: u8, vtime: u8, canonical: bool) -> bool {
    let Ok(mut tio) = tcgetattr(slave) else {
        return false;
    };
    if canonical {
        tio.local_modes.insert(LocalModes::ICANON);
    } else {
        tio.local_modes.remove(LocalModes::ICANON);
    }
    tio.local_modes.remove(LocalModes::ECHO);
    tio.special_codes[SpecialCodeIndex::VMIN] = vmin;
    tio.special_codes[SpecialCodeIndex::VTIME] = vtime;
    tcsetattr(slave, OptionalActions::Now, &tio).is_ok()
        && tcgetattr(slave).is_ok_and(|back| {
            back.local_modes.contains(LocalModes::ICANON) == canonical
                && back.special_codes[SpecialCodeIndex::VMIN] == vmin
                && back.special_codes[SpecialCodeIndex::VTIME] == vtime
        })
}

/// Sleep out a whole number of milliseconds; only the collection loop at the
/// end ever calls it.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: a `timespec` this call owns, and a null second pointer.
    unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
}

/// One timed read, reporting the count (or the refusal's number) and the
/// elapsed milliseconds, with the bytes handed back as text.
fn timed_read(fd: BorrowedFd<'_>) -> (Result<usize, i32>, u64, [u8; 16]) {
    let mut buf = [0u8; 16];
    let at = Instant::now();
    let got = rustix::io::read(fd, &mut buf).map_err(Errno::raw_os_error);
    let ms = at.elapsed().as_millis() as u64;
    (got, ms, buf)
}

fn main() {
    println!("t-tty-nonblock");
    let mut r = Report::new();

    r.check("a SIGALRM handler installs", install_alarm());

    // --- the table, on a slave opened nonblocking ---
    let (master, slave) = fresh_pair(OFlags::NONBLOCK);

    r.check(
        "raw mode with 0/0 is taken",
        set_mode(slave.as_fd(), 0, 0, false),
    );
    let (got, _, _) = timed_read(slave.as_fd());
    r.check(
        "VMIN=0/VTIME=0 answers 0 bytes, as the poll it always was",
        got == Ok(0),
    );

    // Each claim that a *blocking* read would have parked through is guarded by
    // an alarm: without the fix that read answers `EINTR` (or never returns at
    // all), and a hang here would look like a kernel crash rather than a claim.
    r.check(
        "raw mode with 1/0 is taken",
        set_mode(slave.as_fd(), 1, 0, false),
    );
    alarm(Some(300));
    let (got, ms, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "VMIN=1/VTIME=0 with nothing queued answers EAGAIN",
        got == Err(libc::EAGAIN),
    );
    r.check("without waiting for it", ms < 200);

    r.check(
        "raw mode with 0/5 is taken",
        set_mode(slave.as_fd(), 0, 5, false),
    );
    alarm(Some(300));
    let (got, ms, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "VMIN=0/VTIME=5 answers EAGAIN too",
        got == Err(libc::EAGAIN),
    );
    r.check("and the half-second clock was never consulted", ms < 200);

    r.check(
        "raw mode with 2/5 is taken",
        set_mode(slave.as_fd(), 2, 5, false),
    );
    alarm(Some(300));
    let (got, _, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "a nonblocking read with nothing queued is EAGAIN",
        got == Err(libc::EAGAIN),
    );

    // A minimum is a floor on a *wait*: with a byte queued, a nonblocking read
    // that asked for two hands over the one it has.
    r.check(
        "one byte is typed at the slave",
        rustix::io::write(&master, b"x") == Ok(1),
    );
    alarm(Some(300));
    let (got, ms, buf) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "VMIN=2 with one byte queued hands that byte over",
        got == Ok(1),
    );
    r.check("and it is the byte", got == Ok(1) && &buf[..1] == b"x");
    r.check("immediately", ms < 200);

    // Canonical consults neither `VMIN` nor `VTIME`, and does not need the
    // clock: a half-typed line is not an answer.
    r.check(
        "canonical mode with 1/5 is taken",
        set_mode(slave.as_fd(), 1, 5, true),
    );
    r.check(
        "three bytes are typed",
        rustix::io::write(&master, b"abc") == Ok(3),
    );
    alarm(Some(300));
    let (got, _, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "a partial line is EAGAIN even canonically",
        got == Err(libc::EAGAIN),
    );
    r.check(
        "and the newline completes it",
        rustix::io::write(&master, b"\r") == Ok(1),
    );
    let (got, _, buf) = timed_read(slave.as_fd());
    r.check("a complete line is the line", got == Ok(4));
    r.check(
        "and it reads back as one",
        got == Ok(4) && &buf[..4] == b"abc\n",
    );
    drop(slave);
    drop(master);

    // --- the plumbing: fcntl, and which end of the pair the bit is on ---
    let (master, slave) = fresh_pair(OFlags::empty());
    r.check("raw 1/0 on it", set_mode(slave.as_fd(), 1, 0, false));

    // The counterfactual: with no flag anywhere, the same read parks - so the
    // `EAGAIN` below is the flag's doing and not the state's.
    let before = RAN.load(Ordering::Relaxed);
    alarm(Some(200));
    let (got, _, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "a blocking terminal read still parks on a signal",
        got == Err(libc::EINTR),
    );
    r.check(
        "and the handler ran while it was parked",
        RAN.load(Ordering::Relaxed) == before + 1,
    );

    // The master's `O_NONBLOCK` is the master's: `PtyMaster::as_tty` answers
    // the slave's terminal, so a routing mistake here would make the slave
    // nonblocking too and this read would answer `EAGAIN` instead of parking.
    let flags = rustix::fs::fcntl_getfl(&master);
    r.check("the master's flags are readable", flags.is_ok());
    let flags = flags.unwrap_or(OFlags::empty());
    r.check(
        "the master's O_NONBLOCK is settable",
        rustix::fs::fcntl_setfl(&master, flags | OFlags::NONBLOCK).is_ok(),
    );
    alarm(Some(200));
    let (got, _, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "the master's O_NONBLOCK leaves the slave's read blocking",
        got == Err(libc::EINTR),
    );

    // And the slave's own `F_SETFL` reaches the node, where the read consults
    // it: `open(O_NONBLOCK)` is not the only route a program takes. The read
    // below is guarded by an alarm as well, so a kernel that dropped the flag
    // would answer `EINTR` here rather than hanging the sweep.
    let flags = rustix::fs::fcntl_getfl(&slave);
    r.check("the slave's flags are readable", flags.is_ok());
    let flags = flags.unwrap_or(OFlags::empty());
    r.check(
        "the slave's O_NONBLOCK is settable",
        rustix::fs::fcntl_setfl(&slave, flags | OFlags::NONBLOCK).is_ok(),
    );
    alarm(Some(300));
    let (got, ms, _) = timed_read(slave.as_fd());
    alarm(None);
    r.check(
        "fcntl(F_SETFL, O_NONBLOCK) reaches the slave's read",
        got == Err(libc::EAGAIN),
    );
    r.check("which no longer waits", ms < 200);

    // The master's own read honours its own bit, and still carries bytes: what
    // is printed at the slave is read at the master.
    alarm(Some(300));
    let (got, _, _) = timed_read(master.as_fd());
    alarm(None);
    r.check(
        "the master's read is nonblocking too",
        got == Err(libc::EAGAIN),
    );
    // What a program *writes* to its terminal comes out of the master, and the
    // line feed leaves as the `CR LF` pair `ONLCR` exists for: four bytes on
    // the wire from three in the caller's buffer, which is also the case the
    // partial-write count has to answer in the writer's own units.
    r.check(
        "a line is written to the slave",
        rustix::io::write(&slave, b"hi\n") == Ok(3),
    );
    let mut collected = [0u8; 16];
    let mut filled = 0;
    for _ in 0..8 {
        if filled >= 4 {
            break;
        }
        match rustix::io::read(&master, &mut collected[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => sleep_ms(20),
        }
    }
    r.check("the master reads the line the slave wrote", filled == 4);
    r.check(
        "with its newline expanded to CR LF",
        filled == 4 && &collected[..4] == b"hi\r\n",
    );
    drop(slave);
    drop(master);

    let mut back: libc::sigaction = unsafe { std::mem::zeroed() };
    back.sa_sigaction = libc::SIG_DFL;
    // SAFETY: the default disposition for a number the wrapper serves.
    unsafe { libc::sigaction(libc::SIGALRM, &back, std::ptr::null_mut()) };
    r.finish()
}
