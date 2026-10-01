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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

static PTMX: [u8; 10] = *b"/dev/ptmx\0";
static mut PATH: [u8; 20] = [0; 20];
static mut LINE: [u8; 16] = [0; 16];

/// How many times the alarm handler ran: every one of them is an interrupted
/// wait, which is what a *blocking* read of an empty queue is.
static RAN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_alarm(_sig: i32, _info: *mut lin::SigInfo, _ctx: *mut u8) {
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// musl's `__restore_rt`, for a kernel with no libc.
extern "C" fn restorer() {
    let _ = lin::rt_sigreturn();
}

/// A handler with no `SA_RESTART`, so an interrupted read comes back `EINTR` -
/// the guard that turns "this call parked" into a failing check instead of a
/// test that hangs.
fn install_alarm() -> isize {
    let act = lin::SigAction {
        handler: on_alarm as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER,
        restorer: restorer as extern "C" fn() as usize,
        mask: 0,
    };
    lin::rt_sigaction(lin::SIGALRM, &act, core::ptr::null_mut())
}

/// Arm `ITIMER_REAL` once, `ms` from now, or disarm it with `None`.
fn alarm(ms: Option<u64>) {
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
    let at = ms.unwrap_or(0);
    let next = lin::Itimerval {
        it_interval: lin::TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: lin::TimeVal {
            tv_sec: (at / 1000) as i64,
            tv_usec: ((at % 1000) as i64) * 1000,
        },
    };
    let _ = lin::setitimer(lin::ITIMER_REAL, Some(&next), &mut spent);
}

/// `/dev/pts/<n>` for the number `TIOCGPTN` answered.
fn slave_path(n: usize) -> &'static [u8] {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PATH) };
    let path = lin::pts_path(n, buf);
    unsafe { core::slice::from_raw_parts(path.as_ptr(), path.len()) }
}

/// A fresh pty pair. The slave is opened with the flags asked for, which is one
/// of the two routes the bit can take.
fn pair(slave_flags: usize) -> (i32, i32) {
    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0) as i32;
    let mut number: u32 = 0;
    if lin::ioctl(master, lin::TIOCGPTN, &raw mut number as usize) != 0 {
        lin::die(b"t-tty-nonblock: TIOCGPTN failed\n");
    }
    let zero: i32 = 0;
    if lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize) != 0 {
        lin::die(b"t-tty-nonblock: TIOCSPTLCK failed\n");
    }
    let slave = lin::openat(
        lin::AT_FDCWD,
        slave_path(number as usize),
        lin::O_RDWR | slave_flags,
        0,
    ) as i32;
    (master, slave)
}

/// Put the terminal in the mode a full-screen program puts it in - no line
/// editing, no echo - with the `VMIN`/`VTIME` pair under test. Reports whether
/// the kernel took the settings, because a test that silently ran in another
/// mode would prove nothing.
fn set_mode(slave: i32, vmin: u8, vtime: u8, canonical: bool) -> bool {
    let Some(mut tio) = lin::tcgetattr(slave) else {
        return false;
    };
    if canonical {
        tio.lflag |= lin::ICANON;
    } else {
        tio.lflag &= !lin::ICANON;
    }
    tio.lflag &= !lin::ECHO;
    tio.cc[lin::VMIN] = vmin;
    tio.cc[lin::VTIME] = vtime;
    lin::tcsetattr(slave, &tio) == 0
        && lin::tcgetattr(slave).is_some_and(|back| {
            (back.lflag & lin::ICANON != 0) == canonical
                && back.cc[lin::VMIN] == vmin
                && back.cc[lin::VTIME] == vtime
        })
}

/// One timed read, with the buffer cleared first so a failure line shows what
/// actually landed.
fn timed_read(fd: i32) -> (isize, u64) {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
    for b in buf.iter_mut() {
        *b = 0;
    }
    let at = lin::monotonic_ms();
    let got = lin::read(fd, buf);
    (got, lin::monotonic_ms().wrapping_sub(at))
}

/// One timed read, with the bytes read out as text.
fn timed_read_text(fd: i32) -> (isize, u64, &'static [u8]) {
    let (got, ms) = timed_read(fd);
    let text = unsafe {
        let buf = &*core::ptr::addr_of!(LINE);
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        &buf[..len]
    };
    (got, ms, text)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-tty-nonblock\n");
    let mut failed = false;

    let r = install_alarm();
    lin::check_eq(&mut failed, b"a SIGALRM handler installs", r, 0);

    // --- the table, on a slave opened nonblocking ---
    let (master, slave) = pair(lin::O_NONBLOCK);
    lin::check_true(&mut failed, b"a pty pair opens", master >= 0 && slave >= 0);

    lin::check_true(
        &mut failed,
        b"raw mode with 0/0 is taken",
        set_mode(slave, 0, 0, false),
    );
    let (got, _) = timed_read(slave);
    lin::check_eq(
        &mut failed,
        b"VMIN=0/VTIME=0 answers 0 bytes, as the poll it always was",
        got,
        0,
    );

    // Each check that a *blocking* read would have parked through is guarded by
    // an alarm: without the fix that read answers `EINTR` (or never returns at
    // all), and a hang here would look like a kernel crash rather than a check.
    lin::check_true(
        &mut failed,
        b"raw mode with 1/0 is taken",
        set_mode(slave, 1, 0, false),
    );
    alarm(Some(300));
    let (got, ms) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"VMIN=1/VTIME=0 with nothing queued answers EAGAIN",
        got,
        lin::EAGAIN,
    );
    lin::check_true(&mut failed, b"without waiting for it", ms < 200);

    lin::check_true(
        &mut failed,
        b"raw mode with 0/5 is taken",
        set_mode(slave, 0, 5, false),
    );
    alarm(Some(300));
    let (got, ms) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"VMIN=0/VTIME=5 answers EAGAIN too",
        got,
        lin::EAGAIN,
    );
    lin::check_true(
        &mut failed,
        b"and the half-second clock was never consulted",
        ms < 200,
    );

    lin::check_true(
        &mut failed,
        b"raw mode with 2/5 is taken",
        set_mode(slave, 2, 5, false),
    );
    alarm(Some(300));
    let (got, _) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"a nonblocking read with nothing queued is EAGAIN",
        got,
        lin::EAGAIN,
    );

    // A minimum is a floor on a *wait*: with a byte queued, a nonblocking read
    // that asked for two hands over the one it has.
    lin::check_eq(
        &mut failed,
        b"one byte is typed at the slave",
        lin::write(master, b"x"),
        1,
    );
    alarm(Some(300));
    let (got, ms, text) = timed_read_text(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"VMIN=2 with one byte queued hands that byte over",
        got,
        1,
    );
    lin::check_true(&mut failed, b"and it is the byte", text == b"x");
    lin::check_true(&mut failed, b"immediately", ms < 200);

    // Canonical consults neither `VMIN` nor `VTIME`, and does not need the
    // clock: a half-typed line is not an answer.
    lin::check_true(
        &mut failed,
        b"canonical mode with 1/5 is taken",
        set_mode(slave, 1, 5, true),
    );
    lin::check_eq(
        &mut failed,
        b"three bytes are typed",
        lin::write(master, b"abc"),
        3,
    );
    alarm(Some(300));
    let (got, _) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"a partial line is EAGAIN even canonically",
        got,
        lin::EAGAIN,
    );
    lin::check_eq(
        &mut failed,
        b"and the newline completes it",
        lin::write(master, b"\r"),
        1,
    );
    let (got, _, text) = timed_read_text(slave);
    lin::check_eq(&mut failed, b"a complete line is the line", got, 4);
    lin::check_true(&mut failed, b"and it reads back as one", text == b"abc\n");
    let _ = lin::close(slave);
    let _ = lin::close(master);

    // --- the plumbing: fcntl, and which end of the pair the bit is on ---
    let (master, slave) = pair(0);
    lin::check_true(
        &mut failed,
        b"a second pair opens blocking",
        master >= 0 && slave >= 0,
    );
    lin::check_true(&mut failed, b"raw 1/0 on it", set_mode(slave, 1, 0, false));

    // The counterfactual: with no flag anywhere, the same read parks - so the
    // `EAGAIN` below is the flag's doing and not the state's.
    let before = RAN.load(Ordering::Relaxed);
    alarm(Some(200));
    let (got, _) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"a blocking terminal read still parks on a signal",
        got,
        lin::EINTR,
    );
    lin::check_eq(
        &mut failed,
        b"and the handler ran while it was parked",
        RAN.load(Ordering::Relaxed) as isize,
        before as isize + 1,
    );

    // The master's `O_NONBLOCK` is the master's: `PtyMaster::as_tty` answers
    // the slave's terminal, so a routing mistake here would make the slave
    // nonblocking too and this read would answer `EAGAIN` instead of parking.
    let flags = lin::fcntl(master, lin::F_GETFL, 0) as usize;
    lin::check_eq(
        &mut failed,
        b"the master's flags are readable",
        lin::fcntl(master, lin::F_SETFL, flags | lin::O_NONBLOCK),
        0,
    );
    alarm(Some(200));
    let (got, _) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"the master's O_NONBLOCK leaves the slave's read blocking",
        got,
        lin::EINTR,
    );

    // And the slave's own `F_SETFL` reaches the node, where the read consults
    // it: `open(O_NONBLOCK)` is not the only route a program takes. Both of the
    // reads below are guarded by an alarm as well, so a kernel that dropped the
    // flag answers `EINTR` here rather than hanging the sweep.
    let flags = lin::fcntl(slave, lin::F_GETFL, 0) as usize;
    lin::check_eq(
        &mut failed,
        b"the slave's flags are readable",
        lin::fcntl(slave, lin::F_SETFL, flags | lin::O_NONBLOCK),
        0,
    );
    alarm(Some(300));
    let (got, ms) = timed_read(slave);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"fcntl(F_SETFL, O_NONBLOCK) reaches the slave's read",
        got,
        lin::EAGAIN,
    );
    lin::check_true(&mut failed, b"which no longer waits", ms < 200);

    // The master's own read honours its own bit, and still carries bytes: what
    // is printed at the slave is read at the master.
    alarm(Some(300));
    let (got, _) = timed_read(master);
    alarm(None);
    lin::check_eq(
        &mut failed,
        b"the master's read is nonblocking too",
        got,
        lin::EAGAIN,
    );
    // What a program *writes* to its terminal comes out of the master, and the
    // line feed leaves as the `CR LF` pair `ONLCR` exists for: four bytes on
    // the wire from three in the caller's buffer, which is also the case the
    // partial-write count has to answer in the writer's own units.
    lin::check_eq(
        &mut failed,
        b"a line is written to the slave",
        lin::write(slave, b"hi\n"),
        3,
    );
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
    let mut collected = 0;
    for _ in 0..8 {
        if collected >= 4 {
            break;
        }
        for b in buf[collected..].iter_mut() {
            *b = 0;
        }
        let n = lin::read(master, &mut buf[collected..]);
        if n > 0 {
            collected += n as usize;
        } else {
            lin::sleep_ms(20);
        }
    }
    let text = unsafe {
        let buf = &*core::ptr::addr_of!(LINE);
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        &buf[..len]
    };
    lin::check_eq(
        &mut failed,
        b"the master reads the line the slave wrote",
        collected as isize,
        4,
    );
    lin::check_true(
        &mut failed,
        b"with its newline expanded to CR LF",
        text == b"hi\r\n",
    );
    let _ = lin::close(slave);
    let _ = lin::close(master);

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
    lin::die(b"t-tty-nonblock: panicked\n")
}
