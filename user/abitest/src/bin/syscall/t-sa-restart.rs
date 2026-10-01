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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

static PTMX: [u8; 10] = *b"/dev/ptmx\0";
static mut PATH: [u8; 20] = [0; 20];
static mut LINE: [u8; 16] = [0; 16];

/// How many times the handler ran, and the signal it saw. Atomics because
/// nothing on the main line "touches" the handler, and a plain static would let
/// the compiler keep the read of it in a register.
static RAN: AtomicUsize = AtomicUsize::new(0);
static SEEN: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_alarm(sig: i32, _info: *mut lin::SigInfo, _ctx: *mut u8) {
    SEEN.store(sig, Ordering::Relaxed);
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// musl's `__restore_rt`, for a kernel with no libc: the handler's `ret` lands
/// here, and the only thing to do is hand the frame back to `rt_sigreturn`.
extern "C" fn restorer() {
    let _ = lin::rt_sigreturn();
}

/// Give `SIGALRM` a handler with or without `SA_RESTART` - the one flag that
/// separates the two halves of this program.
fn aim_alarm(restart: bool) -> isize {
    let act = lin::SigAction {
        handler: on_alarm as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER | if restart { lin::SA_RESTART } else { 0 },
        restorer: restorer as extern "C" fn() as usize,
        mask: 0,
    };
    lin::rt_sigaction(lin::SIGALRM, &act, core::ptr::null_mut())
}

/// `/dev/pts/<n>` for the number `TIOCGPTN` answered.
fn slave_path(n: usize) -> &'static [u8] {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PATH) };
    let path = lin::pts_path(n, buf);
    unsafe { core::slice::from_raw_parts(path.as_ptr(), path.len()) }
}

/// A fresh pty pair, its slave open for reading. Every check gets its own so a
/// check's leftover bytes cannot be the next one's answer.
fn fresh_pair() -> (i32, i32) {
    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0) as i32;
    let mut number: u32 = 0;
    if lin::ioctl(master, lin::TIOCGPTN, &raw mut number as usize) != 0 {
        lin::die(b"t-sa-restart: TIOCGPTN failed\n");
    }
    let zero: i32 = 0;
    if lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize) != 0 {
        lin::die(b"t-sa-restart: TIOCSPTLCK failed\n");
    }
    let slave = lin::openat(lin::AT_FDCWD, slave_path(number as usize), lin::O_RDWR, 0) as i32;
    (master, slave)
}

/// Type `bytes` at the slave - through the master, the emulator's end - after
/// `delay` ms, from a child, so the read being pinned is already parked when
/// the line lands.
fn type_later(master: i32, delay: u64, bytes: &'static [u8]) -> i32 {
    let pid = lin::fork();
    if pid == 0 {
        lin::sleep_ms(delay);
        let _ = lin::write(master, bytes);
        lin::exit_group(0);
    }
    pid as i32
}

/// One timed read of `fd`, reporting the count, the elapsed milliseconds and
/// the bytes themselves.
fn timed_read(fd: i32) -> (isize, u64, &'static [u8]) {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
    for b in buf.iter_mut() {
        *b = 0;
    }
    let at = lin::monotonic_ms();
    let got = lin::read(fd, buf);
    let ms = lin::monotonic_ms().wrapping_sub(at);
    let text = unsafe {
        let buf = &*core::ptr::addr_of!(LINE);
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        &buf[..len]
    };
    (got, ms, text)
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now: the same route `alarm(2)`
/// takes, and the one a full-screen program's timeout is built on.
fn alarm_once(ms: u64) -> isize {
    let mut spent = lin::Itimerval {
        it_interval: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    let next = lin::Itimerval {
        it_interval: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: lin::TimeVal {
            tv_sec: (ms / 1000) as i64,
            tv_usec: ((ms % 1000) as i64) * 1000,
        },
    };
    lin::setitimer(lin::ITIMER_REAL, Some(&next), &mut spent)
}

fn reap(kid: i32) {
    let mut status = 0i32;
    let _ = lin::wait4(kid as usize, &mut status, 0);
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-sa-restart\n");
    let mut failed = false;

    // 1. `SA_RESTART` set: the line typed at 600 ms is what ends the read, and
    //    the signal due at 200 ms is a handler run that the caller never sees.
    //    Before the rewind existed this read answered `EINTR` at 200 ms, which
    //    is the failure a program that sets the flag and does not check for it
    //    treats as an I/O error.
    let (master, slave) = fresh_pair();
    let r = aim_alarm(true);
    lin::check_eq(
        &mut failed,
        b"a SIGALRM handler installs with SA_RESTART",
        r,
        0,
    );
    let kid = type_later(master, 600, b"ok\r");
    lin::check_eq(&mut failed, b"ITIMER_REAL arms", alarm_once(200), 0);
    let (got, ms, text) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"a restarted read answers the line, not EINTR",
        got,
        3,
    );
    lin::check_true(&mut failed, b"and it is that line", text == b"ok\n");
    lin::check_true(
        &mut failed,
        b"and it waited past the signal for it",
        ms >= 500,
    );
    lin::check_eq(
        &mut failed,
        b"the handler ran once, inside the call",
        RAN.load(Ordering::Relaxed) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"and it ran for SIGALRM",
        SEEN.load(Ordering::Relaxed) as isize,
        lin::SIGALRM as isize,
    );
    reap(kid);
    let _ = lin::close(slave);
    let _ = lin::close(master);

    // 2. The counterfactual: the same shape with `SA_RESTART` clear. The read
    //    comes back `EINTR` at the signal, having consumed nothing, and the
    //    retry gets the line that landed while it was not reading.
    let (master, slave) = fresh_pair();
    let r = aim_alarm(false);
    lin::check_eq(&mut failed, b"SIGALRM is re-armed without SA_RESTART", r, 0);
    let kid = type_later(master, 600, b"ok\r");
    lin::check_eq(&mut failed, b"ITIMER_REAL arms again", alarm_once(200), 0);
    let (got, ms, _) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"without the flag the read comes back EINTR",
        got,
        lin::EINTR,
    );
    lin::check_true(
        &mut failed,
        b"and it came back at the signal, not the line",
        ms < 500,
    );
    lin::check_eq(
        &mut failed,
        b"the handler still ran before the return",
        RAN.load(Ordering::Relaxed) as isize,
        2,
    );
    let (got, ms, text) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"the retried read takes the whole line",
        got,
        3,
    );
    lin::check_true(&mut failed, b"and it is that line", text == b"ok\n");
    lin::check_true(
        &mut failed,
        b"and the interrupted read had consumed nothing",
        ms >= 200,
    );
    reap(kid);
    let _ = lin::close(slave);
    let _ = lin::close(master);

    // 3. A never-restarted call, with the flag *set*: `poll` answers `EINTR`
    //    whatever the disposition asked for. Linux's rule, and the reason the
    //    restartable set is an allow-list: rewinding a multiplexer would be a
    //    handler loop that never returns.
    let (master, slave) = fresh_pair();
    let r = aim_alarm(true);
    lin::check_eq(&mut failed, b"SIGALRM is set with SA_RESTART again", r, 0);
    lin::check_eq(
        &mut failed,
        b"ITIMER_REAL arms once more",
        alarm_once(150),
        0,
    );
    let mut fds = [lin::PollFd {
        fd: slave,
        events: 0x001, // POLLIN
        revents: 0,
    }];
    let at = lin::monotonic_ms();
    let got = lin::poll(&mut fds, -1);
    let ms = lin::monotonic_ms().wrapping_sub(at);
    lin::check_eq(
        &mut failed,
        b"poll answers EINTR even under SA_RESTART",
        got,
        lin::EINTR,
    );
    lin::check_true(&mut failed, b"at the signal, not later", ms < 500);
    lin::check_eq(
        &mut failed,
        b"and its handler ran too",
        RAN.load(Ordering::Relaxed) as isize,
        3,
    );
    let _ = lin::close(slave);
    let _ = lin::close(master);

    // 4. `nanosleep` is on the same never-restarted list, and it hands the
    //    caller the time that was left - which is the only thing that makes a
    //    retry cheap. A 2 s sleep interrupted at 150 ms comes back about 1.85 s
    //    short, and the loop musl's `sleep()` writes therefore sleeps what it
    //    asked for rather than twice it.
    lin::check_eq(
        &mut failed,
        b"the handler is still installed",
        aim_alarm(true),
        0,
    );
    let req = lin::TimeSpec {
        tv_sec: 2,
        tv_nsec: 0,
    };
    let mut rem = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    lin::check_eq(
        &mut failed,
        b"ITIMER_REAL arms for the sleep",
        alarm_once(150),
        0,
    );
    let at = lin::monotonic_ms();
    let got = lin::nanosleep(&req, &mut rem);
    let ms = lin::monotonic_ms().wrapping_sub(at);
    lin::check_eq(
        &mut failed,
        b"nanosleep answers EINTR under SA_RESTART",
        got,
        lin::EINTR,
    );
    lin::check_true(&mut failed, b"at the signal", ms < 1000);
    lin::check_eq(
        &mut failed,
        b"the handler ran for the sleep as well",
        RAN.load(Ordering::Relaxed) as isize,
        4,
    );
    lin::check_true(
        &mut failed,
        b"and the remainder is the time that was left",
        rem.tv_sec == 1 && (0..1_000_000_000).contains(&rem.tv_nsec),
    );

    // Leave the signal the way it was found: a disposition left installed here
    // would run into whatever the next program expects of SIGALRM.
    let back = lin::SigAction {
        handler: 0,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    let _ = lin::rt_sigaction(lin::SIGALRM, &back, core::ptr::null_mut());

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-sa-restart: panicked\n")
}
