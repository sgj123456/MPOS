//! t-sa-restart - `SA_RESTART`: an interrupted call the disposition asked to be
//! run again is *run again*, and the calls Linux never restarts are not.
//!
//! This is the other half of `t-tty-eintr`. That test pins the rule every
//! caller depends on - a trapped signal ends a blocked read with `EINTR`, so a
//! full-screen program's own timeout breaks the wait - and this one pins what
//! happens when the caller said `SA_RESTART`: it never sees the `EINTR` at all.
//! Measured on Linux 7.2 with a terminal read whose `SIGALRM` fired at 100 ms
//! and whose byte arrived at 400 ms: with the flag set the handler runs at
//! 100 ms and the *read* returns the byte at 400 ms; with it clear the same read
//! answers `-1/EINTR` at 100 ms and a retry gets the byte.
//!
//! The kernel's side of that is a rewind, not a repeat of the syscall: the
//! frame the handler runs on is `rt_sigreturn`'s record of the interrupted
//! call, so pointing its saved `rip` back at the `syscall` instruction with the
//! call's number in `rax` is what makes the return re-enter the call - with the
//! caller's own arguments, which that same frame restores.
//!
//! The flag is only half the rule and the other half is per-syscall, so the
//! never-restarted set is pinned too: measured on the same host, `poll(2)` and
//! `nanosleep(2)` answer `-1/EINTR` at 100 ms *with* `SA_RESTART` set.
//! `nanosleep` also carries the remainder back in its second pointer, which is
//! what makes a retry cost the rest of the sleep rather than the whole of it -
//! musl's `sleep()` is `nanosleep(&ts, &ts)` in a loop, so that pointer is the
//! difference between a signal lengthening a sleep and dividing it.
//!
//! # The doors, and the two the libraries do not name
//!
//! The install is `libc::sigaction`, whose musl build supplies the restorer
//! word itself. The interrupted read is `rustix::io::read` - a plain `read`,
//! with no `EINTR` retry in its backend, which is the whole point of pinning it.
//! The multiplexer is `rustix::event::poll`, which is `ppoll` on *both*
//! machines: the call `t-poll` pins, `poll` itself, has no arm64 number, and a
//! null timeout is `-1`'s spelling. The sleep is `libc::nanosleep` rather than
//! `rustix::thread::nanosleep` for exactly one reason: rustix's takes only the
//! request, so the *remainder* - which is one of this program's claims - has
//! nowhere to go through it.
//!
//! Two numbers here come from no maintained table. `ITIMER_REAL` is not
//! defined for any machine this crate builds for (measured: this libc defines
//! it under fuchsia, hurd, aix and nto and in no `linux_like` file), and
//! `TIOCGPTN`/`TIOCSPTLCK` are named by no libc at all - they are computed the
//! way `asm-generic/ioctls.h` computes them (`_IOR('T', 0x30, unsigned int)`
//! and `_IOW('T', 0x31, int)`) and were checked against a compiled probe on
//! this host. The ioctl itself goes through `libc::syscall` for `t-vt`'s
//! reason: `libc::ioctl`'s request parameter is `c_int` in the musl bindings
//! and `c_ulong` in the gnu ones, and a request is neither.
//!
//! One wrapper difference is named rather than hidden: musl's `nanosleep` is
//! **not** a binding of `nanosleep` - the crate's own `libc.a` shows it calling
//! `__clock_nanosleep(CLOCK_REALTIME, 0, req, rem)`, once, with no `EINTR`
//! loop, so the wait this program pins on both machines is `clock_nanosleep`,
//! which is the one call mpos serves both spellings from. `t-nanosleep` is
//! where that pair's own difference (`TIMER_ABSTIME`) is pinned.
//!
//! # The fixture
//!
//! Each check gets its own pty pair, minted by opening `/dev/ptmx` and asking
//! `TIOCGPTN` which `/dev/pts/N` is its slave, so one check's leftover bytes
//! cannot be the next one's answer. Both ends are opened `O_NOCTTY`: a test
//! that acquired a controlling terminal by accident would be measuring the
//! session rather than the restart. This is the first program in this suite to
//! open a pty at all, which is why the pair-minting call is spelled out here.
//! The clock is `std::time::Instant` - the *timing* is a fixture, not a claim;
//! the claims are the four inequalities the flag and the call together decide.

use mpos_user_tests::Report;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, waitpid};
use std::ffi::CString;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Instant;

/// `ITIMER_REAL`: the wall-clock timer `alarm(2)` is a shortcut for. This libc
/// names it for fuchsia, hurd, aix and nto and in no `linux_like` file
/// (measured), so it is written here - with the request words below, the only
/// numbers in this program no maintained table carries.
const ITIMER_REAL: libc::c_int = 0;

/// `asm-generic/ioctls.h`'s two pty requests, checked against a compiled probe
/// on this host: `TIOCGPTN` is `_IOR('T', 0x30, unsigned int)` and
/// `TIOCSPTLCK` is `_IOW('T', 0x31, int)`.
const TIOCGPTN: libc::c_ulong = 0x8004_5430;
const TIOCSPTLCK: libc::c_ulong = 0x4004_5431;

/// How many times the handler ran, and the signal it saw. Atomics because
/// nothing on the main line "touches" the handler, and a plain static would let
/// the compiler keep the read of it in a register.
static RAN: AtomicUsize = AtomicUsize::new(0);
static SEEN: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_alarm(sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    SEEN.store(sig, Ordering::Relaxed);
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// Give `SIGALRM` a handler with or without `SA_RESTART` - the one flag that
/// separates the two halves of this program. musl fills the restorer slot and
/// sets `SA_RESTORER` itself, so no trampoline is this program's business.
fn aim_alarm(restart: bool) -> bool {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        on_alarm as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO | if restart { libc::SA_RESTART } else { 0 };
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves.
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) == 0 }
}

/// One ioctl, with `errno` read immediately on failure - `t-vt`'s helper, for
/// the reason given in its comment (`libc::ioctl`'s request is `c_int` in the
/// musl bindings and `c_ulong` in the gnu ones, and a request is neither).
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
/// written down.
fn fresh_pair() -> (OwnedFd, OwnedFd) {
    let master = open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
        .unwrap_or_else(|e| panic!("opening /dev/ptmx: {e}"));
    let mut number: u32 = 0;
    let (rc, err) = ioctl(
        master.as_fd().as_raw_fd(),
        TIOCGPTN,
        &mut number as *mut u32 as usize,
    );
    if rc != 0 {
        panic!("TIOCGPTN failed: errno {err}");
    }
    let unlocked: i32 = 0;
    let (rc, err) = ioctl(
        master.as_fd().as_raw_fd(),
        TIOCSPTLCK,
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

/// Sleep out a whole number of milliseconds; only fixture children and the
/// setup between checks ever call it.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: a `timespec` this call owns, and the second pointer is null
    // ("do not tell me the remainder").
    unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
}

/// Leave through `exit_group`: the code is the claim, and the child's inherited
/// copy of the parent's stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    // SAFETY: the door does not return; `_exit` is the maintained fallback.
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// Type `bytes` at the slave - through the master, the emulator's end - after
/// `delay` ms, from a child, so the read being pinned is already parked when
/// the line lands. The child writes through `rustix` and leaves through the raw
/// `SYS_exit_group`, so it touches neither the allocator nor the parent's
/// buffered stdout.
fn type_later(master: BorrowedFd<'_>, delay_ms: i64, bytes: &'static [u8]) -> i32 {
    // SAFETY: the child either sleeps and writes to a descriptor it owns, or
    // `_exit`s; it shares nothing mutable with the parent.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        sleep_ms(delay_ms);
        let _ = rustix::io::write(master, bytes);
        leave(0);
    }
    pid
}

/// One timed read of `fd`, reporting the answer (the count, or the refusal's
/// number), the elapsed milliseconds and the bytes themselves.
fn timed_read(fd: BorrowedFd<'_>) -> (Result<usize, i32>, u64, [u8; 16]) {
    let mut buf = [0u8; 16];
    let at = Instant::now();
    let got = rustix::io::read(fd, &mut buf).map_err(Errno::raw_os_error);
    let ms = at.elapsed().as_millis() as u64;
    (got, ms, buf)
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now: the same route `alarm(2)`
/// takes, and the one a full-screen program's timeout is built on - with a
/// resolution in milliseconds, which `alarm(2)` does not have.
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
    // SAFETY: two `itimerval`s this process owns, and one of the three clocks
    // `setitimer` names.
    unsafe { libc::setitimer(ITIMER_REAL, &next, &mut spent) == 0 }
}

fn reap(kid: i32) {
    let _ = waitpid(Pid::from_raw(kid), WaitOptions::empty());
}

fn main() {
    println!("t-sa-restart");
    let mut r = Report::new();

    // 1. `SA_RESTART` set: the line typed at 600 ms is what ends the read, and
    //    the signal due at 200 ms is a handler run that the caller never sees.
    //    Before the rewind existed this read answered `EINTR` at 200 ms, which
    //    is the failure a program that sets the flag and does not check for it
    //    treats as an I/O error.
    let (master, slave) = fresh_pair();
    r.check(
        "a SIGALRM handler installs with SA_RESTART",
        aim_alarm(true),
    );
    let kid = type_later(master.as_fd(), 600, b"ok\r");
    r.check("ITIMER_REAL arms", alarm_once(200));
    let (got, ms, buf) = timed_read(slave.as_fd());
    r.check("a restarted read answers the line, not EINTR", got == Ok(3));
    r.check("and it is that line", &buf[..3] == b"ok\n");
    r.check("and it waited past the signal for it", ms >= 500);
    r.check(
        "the handler ran once, inside the call",
        RAN.load(Ordering::Relaxed) == 1,
    );
    r.check(
        "and it ran for SIGALRM",
        SEEN.load(Ordering::Relaxed) == libc::SIGALRM,
    );
    reap(kid);

    // 2. The counterfactual: the same shape with `SA_RESTART` clear. The read
    //    comes back `EINTR` at the signal, having consumed nothing, and the
    //    retry gets the line that landed while it was not reading.
    let (master, slave) = fresh_pair();
    r.check("SIGALRM is re-armed without SA_RESTART", aim_alarm(false));
    let kid = type_later(master.as_fd(), 600, b"ok\r");
    r.check("ITIMER_REAL arms again", alarm_once(200));
    let (got, ms, _) = timed_read(slave.as_fd());
    r.check(
        "without the flag the read comes back EINTR",
        got == Err(libc::EINTR),
    );
    r.check("and it came back at the signal, not the line", ms < 500);
    r.check(
        "the handler still ran before the return",
        RAN.load(Ordering::Relaxed) == 2,
    );
    let (got, ms, buf) = timed_read(slave.as_fd());
    r.check("the retried read takes the whole line", got == Ok(3));
    r.check("and it is that line", &buf[..3] == b"ok\n");
    r.check("and the interrupted read had consumed nothing", ms >= 200);
    reap(kid);

    // 3. A never-restarted call, with the flag *set*: the multiplexer answers
    //    `EINTR` whatever the disposition asked for. Linux's rule, and the
    //    reason the restartable set is an allow-list: rewinding a multiplexer
    //    would be a handler loop that never returns.
    let (master, slave) = fresh_pair();
    r.check("SIGALRM is set with SA_RESTART again", aim_alarm(true));
    r.check("ITIMER_REAL arms once more", alarm_once(150));
    let mut fds = [PollFd::new(&slave, PollFlags::IN)];
    let at = Instant::now();
    let got = poll(&mut fds, None);
    let ms = at.elapsed().as_millis() as u64;
    r.check(
        "poll answers EINTR even under SA_RESTART",
        got == Err(Errno::INTR),
    );
    r.check("at the signal, not later", ms < 500);
    r.check("and its handler ran too", RAN.load(Ordering::Relaxed) == 3);
    // Nothing was typed on this pair; the master stays open until the poll is
    // over because closing it would hang the slave up and end the wait early.
    drop(master);

    // 4. The sleep is on the same never-restarted list, and it hands the caller
    //    the time that was left - which is the only thing that makes a retry
    //    cheap. A 2 s sleep interrupted at 150 ms comes back about 1.85 s short,
    //    and the loop musl's `sleep()` writes therefore sleeps what it asked
    //    for rather than twice it.
    r.check("the handler is still installed", aim_alarm(true));
    let req = libc::timespec {
        tv_sec: 2,
        tv_nsec: 0,
    };
    let mut rem = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    r.check("ITIMER_REAL arms for the sleep", alarm_once(150));
    let at = Instant::now();
    // SAFETY: two `timespec`s this process owns.
    let got = unsafe { libc::nanosleep(&req, &mut rem) };
    let ms = at.elapsed().as_millis() as u64;
    let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    r.check(
        "nanosleep answers EINTR under SA_RESTART",
        got == -1 && err == libc::EINTR,
    );
    r.check("at the signal", ms < 1000);
    r.check(
        "the handler ran for the sleep as well",
        RAN.load(Ordering::Relaxed) == 4,
    );
    r.check(
        "and the remainder is the time that was left",
        rem.tv_sec == 1 && (0..1_000_000_000).contains(&rem.tv_nsec),
    );

    // Leave the signal the way it was found: a disposition left installed here
    // would run into a signal that arrives while this process is being torn
    // down.
    let mut back: libc::sigaction = unsafe { std::mem::zeroed() };
    back.sa_sigaction = libc::SIG_DFL;
    // SAFETY: `back` is the default disposition, and no old record is wanted.
    unsafe { libc::sigaction(libc::SIGALRM, &back, std::ptr::null_mut()) };

    r.finish()
}
