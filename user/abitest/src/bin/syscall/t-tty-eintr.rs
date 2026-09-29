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
//! - just as load-bearing - measured as *not* an interruption for a signal whose
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

/// Give `SIGALRM` a disposition. `handler` of `None` is `SIG_IGN`, which is the
/// other half of the rule this program pins.
fn aim_alarm(handler: Option<usize>) -> isize {
    let act = lin::SigAction {
        handler: handler.unwrap_or(1),
        flags: if handler.is_some() {
            lin::SA_SIGINFO | lin::SA_RESTORER
        } else {
            0
        },
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

/// One timed read of the slave.
fn timed_read(slave: i32) -> (isize, u64, &'static [u8]) {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
    for b in buf.iter_mut() {
        *b = 0;
    }
    let at = lin::monotonic_ms();
    let got = lin::read(slave, buf);
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

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-tty-eintr\n");
    let mut failed = false;

    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"a pty pair opens", master >= 0);
    let master = master as i32;
    let mut number: u32 = 0;
    if lin::ioctl(master, lin::TIOCGPTN, &raw mut number as usize) != 0 {
        lin::die(b"t-tty-eintr: TIOCGPTN failed\n");
    }
    let zero: i32 = 0;
    if lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize) != 0 {
        lin::die(b"t-tty-eintr: TIOCSPTLCK failed\n");
    }
    let slave = lin::openat(lin::AT_FDCWD, slave_path(number as usize), lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"its slave opens", slave >= 0);
    let slave = slave as i32;

    // Reading is what claims the terminal for this process group, which is what
    // a signal aimed at "the program at this terminal" follows.
    let r = aim_alarm(Some(
        on_alarm as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
    ));
    lin::check_eq(&mut failed, b"a SIGALRM handler installs", r, 0);

    // 1. A line typed at 500 ms, a timer due at 200 ms: the read must come back
    //    `EINTR` at the timer, with the handler already run - not sleep to the
    //    line, and not answer the line as if the signal had never happened.
    let kid = type_later(master, 500, b"late\r");
    lin::check_eq(&mut failed, b"ITIMER_REAL arms", alarm_once(200), 0);
    let (got, ms, _) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"a trapped signal ends the blocked read with EINTR",
        got,
        lin::EINTR,
    );
    lin::check_true(
        &mut failed,
        b"the wait ended at the signal, not at the line",
        ms < 450,
    );
    lin::check_eq(
        &mut failed,
        b"the handler ran exactly once before the return",
        RAN.load(Ordering::Relaxed) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"and the signal it ran for is SIGALRM",
        SEEN.load(Ordering::Relaxed) as isize,
        lin::SIGALRM as isize,
    );

    // 2. The retry gets the line. A read that had consumed bytes on its way out
    //    would hand back a truncated one here, and `strace` shows why it must not:
    //    the kernel's own answer for the interrupted read is `ERESTARTSYS` with
    //    nothing transferred.
    let (got, ms, text) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"the retried read takes the whole line typed mid-interrupt",
        got,
        5,
    );
    lin::check_true(&mut failed, b"and it is that line", text == b"late\n");
    lin::check_true(&mut failed, b"and it waited for it", ms >= 100);
    let mut status = 0i32;
    let _ = lin::wait4(kid as usize, &mut status, 0);

    // 3. The same timer with nothing trapped for it: no interruption. This is
    //    the half a naive rule loses - "any pending signal ends the wait" would
    //    break a shell's own read on every job it finishes, and it is not what
    //    Linux does.
    let r = aim_alarm(None);
    lin::check_eq(&mut failed, b"SIGALRM is set to SIG_IGN", r, 0);
    let kid = type_later(master, 400, b"hi\r");
    lin::check_eq(&mut failed, b"ITIMER_REAL arms again", alarm_once(150), 0);
    let (got, ms, text) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"an ignored signal does not interrupt the read",
        got,
        3,
    );
    lin::check_true(&mut failed, b"the line arrives as typed", text == b"hi\n");
    lin::check_true(
        &mut failed,
        b"and it was the line, not the signal, that ended the wait",
        ms >= 300,
    );
    lin::check_eq(
        &mut failed,
        b"no further handler ran",
        RAN.load(Ordering::Relaxed) as isize,
        1,
    );
    let mut status = 0i32;
    let _ = lin::wait4(kid as usize, &mut status, 0);

    // Leave the terminal the way it was found: a disposition left on `SIG_IGN`
    // would hide the next program's alarm failures.
    let back = lin::SigAction {
        handler: 0,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    let _ = lin::rt_sigaction(lin::SIGALRM, &back, core::ptr::null_mut());

    let _ = lin::close(slave);
    let _ = lin::close(master);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-tty-eintr: panicked\n")
}
