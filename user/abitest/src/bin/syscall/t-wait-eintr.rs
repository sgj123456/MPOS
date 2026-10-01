//! t-wait-eintr - every blocking wait a signal can end, and what it answers.
//!
//! `t-tty-eintr` pins the rule on a terminal, which is where it was first
//! needed: a signal the process trapped ends a blocked read with `EINTR`, so a
//! full-screen program's own timeout brings it back to its loop. The kernel
//! then had that rule in exactly one place. Every other blocking node - a
//! pipe's read *and* its write, an eventfd, a signalfd, an inotify watch, a
//! unix socket's datagram queue and its accept queue, and a pty slave's write
//! into a full master buffer - slept through a catchable signal instead:
//! `block_until` woke only for a fatal one. A daemon whose whole job is to
//! answer both its descriptors and its timers therefore stopped answering the
//! timers for as long as its descriptors stayed quiet, which on an idle box is
//! for ever.
//!
//! Each wait is pinned twice, and the second half is the one that keeps this
//! honest: the signal ends the wait with the *handler having run*, and the same
//! call then answers normally when the thing it was waiting for arrives - so a
//! kernel that broke the data path to pass the first check fails the second.
//!
//! The writes are pinned on the partial-count rule rather than on `EINTR`. A
//! write that already stored bytes reports how many and leaves the rest to the
//! caller; `EINTR` is the answer only when nothing was stored, which is the
//! same rule Linux spells `-ERESTARTSYS` inside and a partial return outside.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

static PTMX: [u8; 10] = *b"/dev/ptmx\0";
static mut PATH: [u8; 20] = [0; 20];
static mut LINE: [u8; 16] = [0; 16];
static mut BIG: [u8; 96 * 1024] = [b'x'; 96 * 1024];

/// How many times the alarm handler ran. Every one of them is a wait that a
/// signal ended - which is the whole subject of this program.
static RAN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_alarm(_sig: i32, _info: *mut lin::SigInfo, _ctx: *mut u8) {
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// musl's `__restore_rt`, for a kernel with no libc.
extern "C" fn restorer() {
    let _ = lin::rt_sigreturn();
}

/// `SIGALRM` gets a handler with **no** `SA_RESTART`, so every call below comes
/// back with `EINTR` rather than being rewound - the flag `t-sa-restart` pins
/// the other way round.
fn install_alarm() -> isize {
    let act = lin::SigAction {
        handler: on_alarm as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER,
        restorer: restorer as extern "C" fn() as usize,
        mask: 0,
    };
    lin::rt_sigaction(lin::SIGALRM, &act, core::ptr::null_mut())
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now: the route `alarm(2)` takes.
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

/// `/dev/pts/<n>` for the number `TIOCGPTN` answered.
fn slave_path(n: usize) -> &'static [u8] {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PATH) };
    let path = lin::pts_path(n, buf);
    unsafe { core::slice::from_raw_parts(path.as_ptr(), path.len()) }
}

fn big() -> &'static mut [u8] {
    unsafe { &mut *core::ptr::addr_of_mut!(BIG) }
}

fn line() -> &'static mut [u8; 16] {
    unsafe { &mut *core::ptr::addr_of_mut!(LINE) }
}

const SOCK_DGRAM: usize = 2;
const SOCK_STREAM: usize = 1;
/// `accept(2)` - the harness has `accept4_un` but not the bare call.
const NR_ACCEPT: u64 = 43;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-wait-eintr\n");
    let mut failed = false;

    lin::check_eq(
        &mut failed,
        b"a SIGALRM handler installs",
        install_alarm(),
        0,
    );

    // --- a pipe's read ---
    {
        let mut fds = [0i32; 2];
        lin::check_eq(&mut failed, b"a pipe opens", lin::pipe2(&mut fds, 0), 0);
        let (r, w) = (fds[0], fds[1]);
        let before = RAN.load(Ordering::Relaxed) as isize;
        alarm_once(200);
        let got = lin::read(r, line());
        lin::check_eq(
            &mut failed,
            b"a blocked pipe read is ended by a signal with EINTR",
            got,
            lin::EINTR,
        );
        lin::check_eq(
            &mut failed,
            b"and its handler ran before the return",
            RAN.load(Ordering::Relaxed) as isize,
            before + 1,
        );
        // The positive control: the same read answers when the byte arrives.
        lin::check_eq(
            &mut failed,
            b"the writer puts a byte in",
            lin::write(w, b"p"),
            1,
        );
        let got = lin::read(r, line());
        lin::check_eq(&mut failed, b"and the retried read takes it", got, 1);
        let _ = lin::close(r);
        let _ = lin::close(w);
    }

    // --- a pipe's write, into a full buffer ---
    {
        let mut fds = [0i32; 2];
        lin::check_eq(
            &mut failed,
            b"a second pipe opens",
            lin::pipe2(&mut fds, 0),
            0,
        );
        let (r, w) = (fds[0], fds[1]);
        let total = big().len();
        alarm_once(200);
        let got = lin::write(w, big());
        // No reader has taken anything, so the write filled the pipe's 64 KiB
        // and parked; the signal ends that park and the answer is what landed.
        lin::check_true(
            &mut failed,
            b"a blocked pipe write answers a partial count, not EINTR",
            got > 0 && (got as usize) < total,
        );
        // And the count is honest: the reader finds exactly those bytes. The
        // write end is closed first, so the drain ends in end-of-stream rather
        // than parking on a writer that still holds the pipe open.
        let _ = lin::close(w);
        let mut seen = 0;
        loop {
            let n = lin::read(r, line());
            if n <= 0 {
                break;
            }
            seen += n as usize;
        }
        lin::check_eq(
            &mut failed,
            b"and the reader finds exactly the bytes it reported",
            seen as isize,
            got,
        );
        let _ = lin::close(r);
    }

    // --- an eventfd ---
    {
        let ev = lin::eventfd(0, 0);
        lin::check_true(&mut failed, b"an eventfd opens", ev >= 0);
        let ev = ev as i32;
        let before = RAN.load(Ordering::Relaxed) as isize;
        alarm_once(200);
        let got = lin::read(ev, line());
        lin::check_eq(
            &mut failed,
            b"a blocked eventfd read is ended by a signal",
            got,
            lin::EINTR,
        );
        lin::check_eq(
            &mut failed,
            b"with its handler run",
            RAN.load(Ordering::Relaxed) as isize,
            before + 1,
        );
        lin::check_eq(
            &mut failed,
            b"a count can be written",
            lin::write(ev, &1u64.to_ne_bytes()),
            8,
        );
        let got = lin::read(ev, line());
        lin::check_eq(&mut failed, b"and the retried read takes it", got, 8);
        let _ = lin::close(ev);
    }

    // --- a signalfd ---
    {
        // `SIGUSR1` blocked and *not* pending: the descriptor has nothing to
        // report, so its read parks - and the alarm that ends that park is a
        // different signal, which the descriptor is not asked about.
        let usr1 = 1u64 << (10 - 1);
        let mut old = 0u64;
        lin::check_eq(
            &mut failed,
            b"SIGUSR1 is blocked",
            lin::rt_sigprocmask(lin::SIG_BLOCK, &usr1, &mut old),
            0,
        );
        let fd = lin::signalfd(-1, &usr1.to_ne_bytes(), 8);
        lin::check_true(&mut failed, b"a signalfd opens", fd >= 0);
        let fd = fd as i32;
        let before = RAN.load(Ordering::Relaxed) as isize;
        alarm_once(200);
        let got = lin::read(fd, &mut [0u8; 128]);
        lin::check_eq(
            &mut failed,
            b"a blocked signalfd read is ended by a signal",
            got,
            lin::EINTR,
        );
        lin::check_eq(
            &mut failed,
            b"with its handler run",
            RAN.load(Ordering::Relaxed) as isize,
            before + 1,
        );
        let pid = lin::getpid();
        lin::check_eq(
            &mut failed,
            b"the blocked signal is sent to this thread",
            lin::tgkill(pid, lin::gettid(), 10),
            0,
        );
        let got = lin::read(fd, &mut [0u8; 128]);
        lin::check_eq(
            &mut failed,
            b"and the retried read takes the record",
            got,
            128,
        );
        let _ = lin::close(fd);
        lin::check_eq(
            &mut failed,
            b"SIGUSR1 is unblocked again",
            lin::rt_sigprocmask(lin::SIG_SETMASK, &old, core::ptr::null_mut()),
            0,
        );
    }

    // --- an inotify watch ---
    {
        static PROBE: [u8; 11] = *b"/tmp/probe\0";
        let fd = lin::inotify_init1(0);
        lin::check_true(&mut failed, b"an inotify fd opens", fd >= 0);
        let fd = fd as i32;
        let file = lin::openat(lin::AT_FDCWD, &PROBE, lin::O_CREAT | lin::O_RDWR, 0o644);
        lin::check_true(&mut failed, b"a file to watch exists", file >= 0);
        let file = file as i32;
        let wd = lin::inotify_add_watch(fd, &PROBE, lin::IN_MODIFY);
        lin::check_true(&mut failed, b"the watch is added", wd >= 0);
        let before = RAN.load(Ordering::Relaxed) as isize;
        alarm_once(200);
        let got = lin::read(fd, &mut [0u8; 256]);
        lin::check_eq(
            &mut failed,
            b"a blocked inotify read is ended by a signal",
            got,
            lin::EINTR,
        );
        lin::check_eq(
            &mut failed,
            b"with its handler run",
            RAN.load(Ordering::Relaxed) as isize,
            before + 1,
        );
        lin::check_eq(
            &mut failed,
            b"the watched file is written",
            lin::write(file, b"x"),
            1,
        );
        let got = lin::read(fd, &mut [0u8; 256]);
        lin::check_true(
            &mut failed,
            b"and the retried read takes the event",
            got > 0,
        );
        let _ = lin::close(file);
        let _ = lin::unlinkat(lin::AT_FDCWD, &PROBE, 0);
        let _ = lin::close(fd);
    }

    // --- a unix datagram socket's queue ---
    {
        let mut sv = [0i32; 2];
        let made = lin::socketpair(lin::AF_UNIX, SOCK_DGRAM, 0, &mut sv);
        lin::check_eq(&mut failed, b"a datagram pair is made", made, 0);
        // Every check below reads and writes a descriptor this pair named, so
        // a pair that did not arrive has nothing to say: `sv` would still hold
        // the zeros it started with, and fd 0 is the console.
        if made != 0 {
            sv = [-1, -1];
        }
        if sv[0] >= 0 {
            let before = RAN.load(Ordering::Relaxed) as isize;
            alarm_once(200);
            let got = lin::read(sv[0], line());
            lin::check_eq(
                &mut failed,
                b"a blocked datagram read is ended by a signal",
                got,
                lin::EINTR,
            );
            lin::check_eq(
                &mut failed,
                b"with its handler run",
                RAN.load(Ordering::Relaxed) as isize,
                before + 1,
            );
            lin::check_eq(
                &mut failed,
                b"the peer sends one",
                lin::write(sv[1], b"d"),
                1,
            );
            let got = lin::read(sv[0], line());
            lin::check_eq(&mut failed, b"and the retried read takes it", got, 1);
            let _ = lin::close(sv[0]);
            let _ = lin::close(sv[1]);
        }
    }

    // --- a unix listener's accept queue ---
    {
        static SOCK_TRIM: &[u8] = b"/tmp/t-wait-eintr.sock";
        static SOCK_NAME: &[u8] = b"/tmp/t-wait-eintr.sock\0";
        let fd = lin::socket(lin::AF_UNIX, SOCK_STREAM, 0);
        lin::check_true(&mut failed, b"a stream socket is made", fd >= 0);
        let fd = fd as i32;
        let (addr, len) = lin::SockAddrUn::new(SOCK_TRIM);
        lin::check_eq(
            &mut failed,
            b"it binds a path",
            lin::bind_un(fd, &addr, len),
            0,
        );
        lin::check_eq(&mut failed, b"and listens", lin::listen(fd, 1), 0);
        let before = RAN.load(Ordering::Relaxed) as isize;
        alarm_once(200);
        let got = lin::raw_sys(NR_ACCEPT, fd as usize, 0, 0);
        lin::check_eq(
            &mut failed,
            b"a blocked accept is ended by a signal",
            got,
            lin::EINTR,
        );
        lin::check_eq(
            &mut failed,
            b"with its handler run",
            RAN.load(Ordering::Relaxed) as isize,
            before + 1,
        );
        // The positive control: a client that connects is accepted.
        let kid = lin::fork();
        if kid == 0 {
            let client = lin::socket(lin::AF_UNIX, SOCK_STREAM, 0) as i32;
            lin::sleep_ms(100);
            let _ = lin::connect_un(client, &addr, len);
            lin::sleep_ms(400);
            lin::exit_group(0);
        }
        let got = lin::raw_sys(NR_ACCEPT, fd as usize, 0, 0);
        lin::check_true(
            &mut failed,
            b"and the retried accept takes the client",
            got >= 0,
        );
        if got >= 0 {
            let _ = lin::close(got as i32);
        }
        let mut status = 0i32;
        let _ = lin::wait4(kid as usize, &mut status, 0);
        let _ = lin::close(fd);
        let _ = lin::unlinkat(lin::AT_FDCWD, SOCK_NAME, 0);
    }

    // --- a pty slave's write into a full master buffer ---
    {
        let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0) as i32;
        let mut number: u32 = 0;
        if lin::ioctl(master, lin::TIOCGPTN, &raw mut number as usize) != 0 {
            lin::die(b"t-wait-eintr: TIOCGPTN failed\n");
        }
        let zero: i32 = 0;
        if lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize) != 0 {
            lin::die(b"t-wait-eintr: TIOCSPTLCK failed\n");
        }
        let slave = lin::openat(lin::AT_FDCWD, slave_path(number as usize), lin::O_RDWR, 0) as i32;
        lin::check_true(&mut failed, b"a pty pair opens", master >= 0 && slave >= 0);
        // Nobody reads the master, so 96 KiB of output fills its 64 KiB and the
        // writer parks - where a signal has to reach it too.
        let before = RAN.load(Ordering::Relaxed) as isize;
        alarm_once(200);
        let got = lin::write(slave, big());
        lin::check_true(
            &mut failed,
            b"a blocked pty write answers a partial count",
            got > 0 && (got as usize) < big().len(),
        );
        lin::check_eq(
            &mut failed,
            b"and a signal is what ended it",
            RAN.load(Ordering::Relaxed) as isize,
            before + 1,
        );
        // The positive control: with the master draining, the rest goes.
        let mut drained = 0;
        while drained < 4096 {
            let n = lin::read(master, line());
            if n <= 0 {
                break;
            }
            drained += n;
        }
        lin::check_true(&mut failed, b"the master reads the bytes back", drained > 0);
        let _ = lin::close(slave);
        let _ = lin::close(master);
    }

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
    lin::die(b"t-wait-eintr: panicked\n")
}
