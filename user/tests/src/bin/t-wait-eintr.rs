//! t-wait-eintr - every blocking wait a signal can end, and what it answers.
//!
//! `t-tty-eintr` pins the rule on a terminal, which is where it was first
//! needed: a signal the process trapped ends a blocked read with `EINTR`, so a
//! full-screen program's own timeout brings it back to its loop. The kernel
//! then had that rule in exactly one place. Every other blocking node - a
//! pipe's read *and* its write, an eventfd, a signalfd, an inotify watch, a
//! unix socket's datagram queue and its accept queue, a pty slave's write
//! into a full master buffer, and a `wait4`/`waitid` blocked on a child that is
//! slow to exit - slept through a catchable signal instead: their park woke only
//! for a fatal one. A daemon whose whole job is to answer both its descriptors
//! and its timers therefore stopped answering the timers for as long as its
//! descriptors stayed quiet, which on an idle box is for ever.
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
//!
//! # The migration: the doors, and the one node no library binds
//!
//! The install is `libc::sigaction` (`SA_SIGINFO`, **no** `SA_RESTART`) and the
//! timer is `libc::setitimer(ITIMER_REAL)` - the same pair `t-tty-eintr` and
//! `t-sa-restart` use, musl filling the restorer word itself. The reads and
//! writes are `rustix::io::{read,write}`, plain calls with no `EINTR` retry in
//! the backend (a retry is what would make the `EINTR` unobservable, and on a
//! write that already stored bytes the partial count is the answer either way).
//!
//! Every *node* is `rustix`'s: `rustix::pipe::pipe`, `rustix::event::eventfd`,
//! `rustix::fs::inotify::{init,add_watch}` watching a file this program makes,
//! `rustix::net::socketpair` for the datagram queue, and
//! `rustix::net::{socket,bind,listen,accept}` for the accept queue - as
//! `t-unix-dgram` used them. The two waits are different shapes on purpose:
//! `rustix::process::waitpid` is `wait4`, and it is the `wait` a shell actually
//! writes, so the `EINTR` is read off its `Err(Errno::INTR)`; `waitid` has no
//! `rustix` binding, so it is the raw `SYS_waitid` with the caller's own
//! `libc::siginfo_t`, the door `t-waitid` opened.
//!
//! **The one node no library binds is `signalfd`.** This libc binds none of
//! `signalfd`/`signalfd4` for the targets here (measured), so the descriptor is
//! the raw `SYS_signalfd4`; the mask is a `libc::sigset_t` the libc's own
//! `sigaddset` and `sigprocmask` build, and the delivery that ends the blocked
//! read is the raw `SYS_tgkill` aimed at this thread, which is the sender a
//! `signalfd` reader actually has. `t-signalfd` owns the call's full contract;
//! this program only pins that its *read* is ended by a signal like every other
//! queue's.
//!
//! An alarm that fires while a call is parked is the only thing being asked
//! for, so a check that would otherwise park for ever is a failure the runner
//! reads as a hang - which is why the `EINTR` half of each pair comes first and
//! its positive control second.

use mpos_user_tests::Report;
use rustix::event::{EventfdFlags, eventfd};
use rustix::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use rustix::fs::inotify::{self, WatchFlags};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::{Errno, read, write};
use rustix::net::{
    AddressFamily, SocketAddrUnix, SocketFlags, SocketType, accept, bind, connect, listen, socket,
    socketpair,
};
use rustix::pipe::pipe;
use rustix::process::{Pid, WaitOptions, waitpid};
use std::sync::atomic::{AtomicUsize, Ordering};

/// `ITIMER_REAL`: the wall-clock timer `alarm(2)` is a shortcut for. This libc
/// names it for fuchsia, hurd, aix and nto and in no `linux_like` file
/// (measured), so it is written here - the one number in this program no
/// maintained table carries.
const ITIMER_REAL: libc::c_int = 0;

/// How many times the `SIGALRM` handler ran. Every one of them is a wait that a
/// signal ended - which is the whole subject of this program.
static RAN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_alarm(_sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    RAN.fetch_add(1, Ordering::Relaxed);
}

/// The 96 KiB this program writes into a pipe and a pty, neither of which has
/// room for it: the write stores what it can and parks, which is where the
/// signal has to reach it. A stack array this size would be its own problem.
static BIG: [u8; 96 * 1024] = [b'x'; 96 * 1024];

/// Give `SIGALRM` a handler with **no** `SA_RESTART`, so every call below comes
/// back with `EINTR` rather than being rewound - the flag `t-sa-restart` pins
/// the other way round.
fn install_alarm() -> bool {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        on_alarm as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves.
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) == 0 }
}

/// Arm `ITIMER_REAL` to fire once, `ms` from now: the route `alarm(2)` takes.
///
/// The 50ms every caller passes is a **budget** for "the task is parked": the
/// syscall blocks and the alarm ends it, so the number only has to exceed the
/// time from `setitimer` to the park - microseconds here, a few milliseconds on
/// the second machine's TCG. It was 200ms, and the fixture children slept
/// 1200ms to outlive it; both are 4x smaller now and the claims are the same
/// (`docs/notes/` §15.93).
fn alarm_once(ms: u64) -> bool {
    // `as _` at the field rather than the field's own alias: this libc
    // deprecates `time_t`/`suseconds_t` for a reason that is not this file's
    // (the same note `t-sa-restart`'s helper carries).
    let next = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: (ms / 1000) as i64 as _,
            tv_usec: ((ms % 1000) * 1000) as i64 as _,
        },
    };
    let mut spent = next;
    // SAFETY: two `itimerval`s this process owns, and the one clock named.
    unsafe { libc::setitimer(ITIMER_REAL, &next, &mut spent) == 0 }
}

/// Sleep out a whole number of milliseconds; only the fixture children and the
/// drain between checks ever call it.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: a `timespec` this call owns, and a null remainder pointer.
    unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
}

/// Leave through `exit_group`: the code is the claim, and a forked child's
/// inherited copy of this process's stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    // SAFETY: the door does not return; `_exit` is the maintained fallback.
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// One ioctl, with `errno` read immediately on failure - `t-sa-restart`'s
/// helper, for the reason in its comment (`libc::ioctl`'s request is `c_int` in
/// the musl bindings and `c_ulong` in the gnu ones, and a request is neither).
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
/// written down - a check's leftover bytes must not be the next check's answer.
/// `None` is a host with no `devpts` (the fixture cannot be built there);
/// `t-pty`'s `mint` returns the same way and for the same reason.
fn fresh_pair() -> Option<(OwnedFd, OwnedFd)> {
    let master = open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).ok()?;
    let mut number: u32 = 0;
    let (rc, _) = ioctl(
        master.as_fd().as_raw_fd(),
        libc::TIOCGPTN as libc::c_ulong,
        &mut number as *mut u32 as usize,
    );
    if rc != 0 {
        return None;
    }
    let unlocked: i32 = 0;
    let (rc, _) = ioctl(
        master.as_fd().as_raw_fd(),
        libc::TIOCSPTLCK as libc::c_ulong,
        &unlocked as *const i32 as usize,
    );
    if rc != 0 {
        return None;
    }
    let path = std::ffi::CString::new(format!("/dev/pts/{number}")).expect("a path with no NUL");
    let slave = open(
        path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .ok()?;
    Some((master, slave))
}

/// `SIGUSR1` as a `libc::sigset_t` word, the mask the signalfd check hands the
/// kernel. The bit arithmetic stays the libc's business.
fn usr1_set() -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a `sigset_t` this process owns.
    unsafe { libc::sigaddset(&mut set, libc::SIGUSR1) };
    set
}

/// `waitid(idtype, id, infop, options, rusage)` through the raw door, with the
/// caller's own record. Returns the answer, or the negative errno - `t-waitid`'s
/// helper, and the same door (this crate binds no `waitid`).
fn waitid(idtype: u32, id: i32, info: &mut libc::siginfo_t, options: i32) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_waitid,
            idtype as libc::c_long,
            id as libc::c_long,
            info as *mut libc::siginfo_t as libc::c_long,
            options as libc::c_long,
            0 as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

fn main() {
    println!("t-wait-eintr");
    let mut r = Report::new();

    r.check("a SIGALRM handler installs", install_alarm());

    // === a pipe's read =====================================================
    {
        let (rd, wr) = pipe().unwrap_or_else(|e| panic!("pipe: {e}"));
        let before = RAN.load(Ordering::Relaxed);
        r.check("ITIMER_REAL arms", alarm_once(50));
        let mut buf = [0u8; 16];
        let got = read(&rd, &mut buf).map_err(Errno::raw_os_error);
        r.check(
            "a blocked pipe read is ended by a signal with EINTR",
            got == Err(libc::EINTR),
        );
        r.check(
            "and its handler ran before the return",
            RAN.load(Ordering::Relaxed) == before + 1,
        );
        // The positive control: the same read answers when the byte arrives.
        r.check("the writer puts a byte in", write(&wr, b"p") == Ok(1));
        let got = read(&rd, &mut buf).map_err(Errno::raw_os_error);
        r.check("and the retried read takes it", got == Ok(1));
    }

    // === a pipe's write, into a full buffer ================================
    {
        let (rd, wr) = pipe().unwrap_or_else(|e| panic!("pipe: {e}"));
        let total = BIG.len();
        let before = RAN.load(Ordering::Relaxed);
        r.check("ITIMER_REAL arms for the write", alarm_once(50));
        // No reader has taken anything, so the write filled the pipe and
        // parked; the signal ends that park and the answer is what landed.
        let got = write(&wr, &BIG);
        r.check(
            "a blocked pipe write answers a partial count, not EINTR",
            matches!(got, Ok(n) if n > 0 && n < total),
        );
        r.check(
            "and a signal is what ended it",
            RAN.load(Ordering::Relaxed) == before + 1,
        );
        // And the count is honest: the reader finds exactly those bytes. The
        // write end is closed first, so the drain ends in end-of-stream rather
        // than parking on a writer that still holds the pipe open.
        drop(wr);
        let mut seen = 0usize;
        let mut buf = [0u8; 4096];
        loop {
            match read(&rd, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen += n,
            }
        }
        r.check(
            "and the reader finds exactly the bytes it reported",
            matches!(got, Ok(n) if n == seen),
        );
    }

    // === an eventfd ========================================================
    {
        let ev = eventfd(0, EventfdFlags::empty()).unwrap_or_else(|e| panic!("eventfd: {e}"));
        let before = RAN.load(Ordering::Relaxed);
        r.check("ITIMER_REAL arms for the eventfd", alarm_once(50));
        let mut buf = [0u8; 8];
        let got = read(&ev, &mut buf).map_err(Errno::raw_os_error);
        r.check(
            "a blocked eventfd read is ended by a signal",
            got == Err(libc::EINTR),
        );
        r.check(
            "with its handler run",
            RAN.load(Ordering::Relaxed) == before + 1,
        );
        r.check(
            "a count can be written",
            write(&ev, &1u64.to_ne_bytes()[..]) == Ok(8),
        );
        let got = read(&ev, &mut buf).map_err(Errno::raw_os_error);
        r.check("and the retried read takes it", got == Ok(8));
    }

    // === a signalfd ========================================================
    {
        // `SIGUSR1` blocked and *not* pending: the descriptor has nothing to
        // report, so its read parks - and the alarm that ends that park is a
        // different signal, which the descriptor is not asked about.
        let set = usr1_set();
        // SAFETY: a set this process owns, and no old set is wanted.
        r.check(
            "SIGUSR1 is blocked",
            unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) } == 0,
        );
        // SAFETY: a set this process owns; -1 makes a new descriptor.
        let raw = unsafe { libc::signalfd(-1, &set, 0) };
        r.check("a signalfd opens", raw >= 0);
        if raw >= 0 {
            // SAFETY: the call just handed this process a fresh descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let before = RAN.load(Ordering::Relaxed);
            r.check("ITIMER_REAL arms for the signalfd", alarm_once(50));
            let mut buf = [0u8; 128];
            let got = read(&fd, &mut buf).map_err(Errno::raw_os_error);
            r.check(
                "a blocked signalfd read is ended by a signal",
                got == Err(libc::EINTR),
            );
            r.check(
                "with its handler run",
                RAN.load(Ordering::Relaxed) == before + 1,
            );
            let pid = rustix::process::getpid().as_raw_nonzero().get();
            let tid = rustix::thread::gettid().as_raw_nonzero().get();
            // SAFETY: two ids this process owns, and the one signal in the mask.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_tgkill,
                    pid as libc::c_long,
                    tid as libc::c_long,
                    libc::SIGUSR1 as libc::c_long,
                )
            };
            r.check("the blocked signal is sent to this thread", sent == 0);
            let got = read(&fd, &mut buf).map_err(Errno::raw_os_error);
            r.check("and the retried read takes the record", got == Ok(128));
        }
        r.check(
            "SIGUSR1 is unblocked again",
            unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) } == 0,
        );
    }

    // === an inotify watch ==================================================
    {
        let path = c"/tmp/t-wait-eintr-probe";
        let inot = inotify::init(inotify::CreateFlags::empty())
            .unwrap_or_else(|e| panic!("inotify_init: {e}"));
        let file = open(
            path,
            OFlags::CREATE | OFlags::RDWR,
            Mode::from_bits_truncate(0o644),
        )
        .unwrap_or_else(|e| panic!("opening the watched file: {e}"));
        let wd = inotify::add_watch(&inot, path, WatchFlags::MODIFY);
        r.check("the watch is added", wd.is_ok());
        let before = RAN.load(Ordering::Relaxed);
        r.check("ITIMER_REAL arms for the inotify", alarm_once(50));
        let mut buf = [0u8; 256];
        let got = read(&inot, &mut buf).map_err(Errno::raw_os_error);
        r.check(
            "a blocked inotify read is ended by a signal",
            got == Err(libc::EINTR),
        );
        r.check(
            "with its handler run",
            RAN.load(Ordering::Relaxed) == before + 1,
        );
        r.check("the watched file is written", write(&file, b"x") == Ok(1));
        let got = read(&inot, &mut buf).map_err(Errno::raw_os_error);
        r.check(
            "and the retried read takes the event",
            matches!(got, Ok(n) if n > 0),
        );
        let _ = rustix::fs::unlink(path);
    }

    // === a unix datagram socket's queue ====================================
    {
        let (a, b) = socketpair(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap_or_else(|e| panic!("socketpair: {e}"));
        let before = RAN.load(Ordering::Relaxed);
        r.check("ITIMER_REAL arms for the datagram read", alarm_once(50));
        let mut buf = [0u8; 16];
        let got = read(&a, &mut buf).map_err(Errno::raw_os_error);
        r.check(
            "a blocked datagram read is ended by a signal",
            got == Err(libc::EINTR),
        );
        r.check(
            "with its handler run",
            RAN.load(Ordering::Relaxed) == before + 1,
        );
        r.check("the peer sends one", write(&b, b"d") == Ok(1));
        let got = read(&a, &mut buf).map_err(Errno::raw_os_error);
        r.check("and the retried read takes it", got == Ok(1));
    }

    // === a unix listener's accept queue ====================================
    {
        let name = c"/tmp/t-wait-eintr-listen.sock";
        let listener = socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .unwrap_or_else(|e| panic!("socket: {e}"));
        let addr = SocketAddrUnix::new(name.to_str().expect("utf-8 path"))
            .unwrap_or_else(|e| panic!("building the address: {e}"));
        r.check("it binds a path", bind(&listener, &addr).is_ok());
        r.check("and listens", listen(&listener, 1).is_ok());
        let before = RAN.load(Ordering::Relaxed);
        r.check("ITIMER_REAL arms for the accept", alarm_once(50));
        let got = accept(&listener);
        r.check(
            "a blocked accept is ended by a signal",
            matches!(&got, Err(e) if *e == Errno::INTR),
        );
        r.check(
            "with its handler run",
            RAN.load(Ordering::Relaxed) == before + 1,
        );
        // The positive control: a client that connects is accepted. The child
        // leaves through the raw `exit_group`, so it never flushes the parent's
        // buffered stdout a second time.
        // SAFETY: the child either sleeps and connects to a name it owns, or
        // `_exit`s; it shares nothing mutable with the parent.
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            sleep_ms(30);
            if let Ok(client) = socket(AddressFamily::UNIX, SocketType::STREAM, None) {
                let _ = connect(&client, &addr);
                sleep_ms(50);
            }
            leave(0);
        }
        let got = accept(&listener);
        r.check("and the retried accept takes the client", got.is_ok());
        let _ = waitpid(Pid::from_raw(kid), WaitOptions::empty());
        let _ = rustix::fs::unlink(name);
    }

    // === a pty slave's write into a full master buffer =====================
    // The fixture is a pty pair, and a host without `devpts` cannot build one -
    // there the claim is not made (named, not hidden); this kernel always can,
    // so a missing pair is a failed check rather than a skipped one.
    match fresh_pair() {
        Some((master, slave)) => {
            // Nobody reads the master, so the output fills its buffer and the
            // writer parks - where a signal has to reach it too.
            let before = RAN.load(Ordering::Relaxed);
            r.check("ITIMER_REAL arms for the pty write", alarm_once(50));
            let got = write(&slave, &BIG);
            r.check(
                "a blocked pty write answers a partial count",
                matches!(got, Ok(n) if n > 0 && n < BIG.len()),
            );
            r.check(
                "and a signal is what ended it",
                RAN.load(Ordering::Relaxed) == before + 1,
            );
            // The positive control: with the master draining, the bytes come
            // back.
            let mut drained = 0usize;
            let mut buf = [0u8; 4096];
            while drained < 4096 {
                match read(&master, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => drained += n,
                }
            }
            r.check("the master reads the bytes back", drained > 0);
        }
        None if mpos_user_tests::host_run() => {
            println!("note: /dev/ptmx is absent on this host; the pty-write claim is not made");
        }
        None => r.check("a pty pair opens for the write check", false),
    }

    // === a wait4 blocked on a child that is slow to exit ===================
    // The gap §2.5 named: `wait4`/`waitid` parked on the fatal-only primitive,
    // so a caught signal woke the task and the park then swallowed the wake. A
    // shell that traps some signal while waiting on a child must get it back as
    // `EINTR` (and be restarted under `SA_RESTART`) - the `SA_RESTART` half is
    // pinned in `t-sa-restart`; the `EINTR` half is here.
    {
        // SAFETY: the child either sleeps and leaves, or `_exit`s.
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            sleep_ms(200);
            leave(3);
        }
        r.check("a child is forked for wait4", kid > 0);
        if kid > 0 {
            let before = RAN.load(Ordering::Relaxed);
            r.check("ITIMER_REAL arms for the wait4", alarm_once(50));
            let got = waitpid(Pid::from_raw(kid), WaitOptions::empty());
            r.check(
                "a blocked wait4 is ended by a catchable signal with EINTR",
                matches!(&got, Err(e) if *e == Errno::INTR),
            );
            r.check(
                "and its handler ran before the return",
                RAN.load(Ordering::Relaxed) == before + 1,
            );
            // The positive control: the retried wait4 still collects the child,
            // so the EINTR came from the signal and not from a broken wait path.
            let got2 = waitpid(Pid::from_raw(kid), WaitOptions::empty());
            r.check(
                "and the retried wait4 takes the child",
                matches!(&got2, Ok(Some((p, _))) if p.as_raw_nonzero().get() == kid),
            );
            r.check(
                "with the child's own exit code",
                matches!(&got2, Ok(Some((_, st))) if st.exit_status() == Some(3)),
            );
        }
    }

    // === a waitid blocked on a child that is slow to exit ==================
    // The same park, reached through `waitid` (`P_ALL`/`WEXITED`): it shares the
    // child-event park, so it must answer the same way. `waitid` reports success
    // as 0 and fills the siginfo.
    {
        // SAFETY: the child either sleeps and leaves, or `_exit`s.
        let kid = unsafe { libc::fork() };
        if kid == 0 {
            sleep_ms(200);
            leave(4);
        }
        r.check("a child is forked for waitid", kid > 0);
        if kid > 0 {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let before = RAN.load(Ordering::Relaxed);
            r.check("ITIMER_REAL arms for the waitid", alarm_once(50));
            let got = waitid(0, 0, &mut info, 4);
            r.check(
                "a blocked waitid is ended by a catchable signal with EINTR",
                got == -(libc::EINTR as isize),
            );
            r.check(
                "and its handler ran before the return",
                RAN.load(Ordering::Relaxed) == before + 1,
            );
            let got2 = waitid(0, 0, &mut info, 4);
            r.check("and the retried waitid still reports the child", got2 == 0);
        }
    }

    // Put the disposition back so nothing this program left armed can end a
    // later check with SIGALRM's default action.
    let back: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a zeroed `sigaction` is `SIG_DFL` with no flags.
    unsafe { libc::sigaction(libc::SIGALRM, &back, std::ptr::null_mut()) };

    r.finish()
}
