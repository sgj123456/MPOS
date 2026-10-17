//! t-pty - one test for the pseudo-terminal pair, the thing a login runs on
//! when it is not on the machine's own console.
//!
//! Pinned here: opening `/dev/ptmx` mints a *new* pair and hands back its
//! master; `TIOCGPTN` on the master names the number the slave appears under
//! in `/dev/pts`; `TIOCSPTLCK` with zero is `unlockpt`, and before it a
//! master still answers. A pair is *one* terminal with two ends, so the
//! terminal ioctls answer on both - the master included, which is how a
//! supervisor that holds nothing else decides it can do job control. Bytes
//! written at the master are typed at the slave's line discipline (canonical
//! mode, so `hi\r` reads back as `hi\n`), and what the slave prints is read at
//! the master - through the output post-processing a terminal owes its
//! emulator, so a printed newline arrives as carriage return and line feed
//! rather than as the line feed that would staircase every line of it. The
//! *window* is the pair's too - `TIOCSWINSZ` on the master is what
//! `TIOCGWINSZ` on the slave reports, which is how an emulator sizes the
//! programs inside it - and a *change* to it is announced, which is the one
//! thing a full-screen program cannot notice for itself. The pair's queues are
//! pinned too: `FIONREAD` answers the *input* queue of whichever end is asked -
//! a completed canonical line at the slave, every byte in raw mode, and what
//! the slave has printed at the master.
//!
//! # The doors
//!
//! The four requests this program is about come from `libc`'s own
//! architecture table (`linux/arch/generic/mod.rs`: `TCGETS` 0x5401,
//! `TIOCGPTN` `u32_cast_ioctl(0x80045430)`, `TIOCSPTLCK` 0x40045431,
//! `TIOCSCTTY` 0x540E) - measured for both of this suite's targets, where the
//! constants' type is `Ioctl`: `c_int` on musl and `c_ulong` on gnu, which is
//! why every one of them is cast into the `c_ulong` the helper takes. The call
//! itself is `libc::syscall(SYS_ioctl)`, for `t-vt`'s reason: `libc::ioctl`'s
//! request parameter is `c_int` in the musl bindings and `c_ulong` in the gnu
//! ones, and a request is neither. The termios and window *structures* are
//! `rustix`'s (`tcgetattr`, `tcgetwinsize`, `tcsetwinsize`), so the field
//! layout and the flag spellings come from a maintained crate rather than
//! from this file's memory of them; `rustix::pty` itself is deliberately not
//! used, because the mint sequence is what this program pins and that module
//! hides every request it is made of (`posix_openpt`, `ptsname_r`).
//!
//! # The fixture, and the one claim that is this kernel's own
//!
//! Each end is opened `O_NOCTTY`, so the test process cannot acquire a
//! terminal by accident - and that is also why the window half runs in a child
//! that *does* acquire the slave, as a session leader whose controlling
//! terminal it becomes (`setsid` + open + `TIOCSCTTY`, the getty's own
//! sequence). Linux sends `SIGWINCH` to a terminal's *foreground process
//! group*, and a pair nobody has claimed has none: measured on a current host,
//! `TIOCSWINSZ` on a fresh pair's master with no session behind it runs no
//! handler at all, and `tcsetpgrp` on a tty that is not the caller's own
//! answers `ENOTTY` (errno 25). This kernel answers the same question with the
//! *reader*: `Tty::read` records the reading task's group as the terminal's
//! foreground (`src/vfs/tty.rs`, "the same answer for a program that never
//! asks"), which is why the no-std version passed without a session. The getty
//! sequence is used here so that the claim holds on **both** machines rather
//! than on one: the divergence itself is named in `docs/uapi/` §5.2 and is
//! not what this program pins.
//!
//! `TIOCGPTN`'s *number* is the one claim that is this kernel's own. This
//! kernel mints pair numbers past its six virtual consoles (`src/vfs/pty.rs`
//! starts its counter at `VC_COUNT + 1`, the consoles holding 1..=6), where
//! Linux's `devpts` numbers from 0 - measured on a current host, which handed
//! back 5. The claim therefore pins this kernel's answer on the payload and
//! accepts the host's under `MPOS_ON_HOST`, printing what the host said.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::termios::{
    LocalModes, OptionalActions, SpecialCodeIndex, Winsize, tcgetattr, tcgetwinsize, tcsetattr,
    tcsetwinsize,
};
use std::ffi::CString;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// What the `SIGWINCH` handler saw. Atomics because the handler runs on the
/// frame the kernel built, and a plain static would let the compiler keep the
/// main line's read of it in a register.
static WINCHED: AtomicUsize = AtomicUsize::new(0);
static WINCH_SIG: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_winch(sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    WINCH_SIG.store(sig, Ordering::Relaxed);
    WINCHED.fetch_add(1, Ordering::Relaxed);
}

/// The alarm's handler: it does nothing but end a wait. Every read of the pair
/// is wrapped in one, so a wire that has stopped carrying bytes is a claim that
/// fails rather than a sweep that hangs.
extern "C" fn on_alarm(_sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {}

/// Arm `ITIMER_REAL` to fire once `ms` from now, or disarm it with `None`.
fn alarm(ms: Option<u64>) {
    let at = ms.unwrap_or(0);
    let next = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: (at / 1000) as _,
            tv_usec: ((at % 1000) * 1000) as _,
        },
    };
    let mut spent = next;
    // SAFETY: two `itimerval`s this process owns, and the one clock that
    // measures real time.
    unsafe { libc::setitimer(0, &next, &mut spent) };
}

/// One ioctl, with `errno` read immediately on failure - `t-sa-restart`'s
/// helper, for the reason its comment gives (`libc::ioctl`'s request parameter
/// is `c_int` in the musl bindings and `c_ulong` in the gnu ones, and a request
/// is neither).
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

/// The master of a fresh pair, its number asked of `TIOCGPTN` the way
/// `posix_openpt`'s caller does, and the pair unlocked. `O_NOCTTY` because a
/// master is never a session's terminal; the slave is where that happens.
fn mint() -> Option<(OwnedFd, u32)> {
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
    Some((master, number))
}

/// The number a pair's slave answers to in `/dev/pts`, spelled the way
/// `ptsname` does.
fn slave_path(number: u32) -> CString {
    CString::new(format!("/dev/pts/{number}")).expect("a path with no NUL in it")
}

/// Put `fd` into canonical or raw mode, the way `stty` does - the line
/// discipline's own switch, which is what `FIONREAD`'s answer turns on.
fn set_canon(fd: BorrowedFd<'_>, on: bool) {
    if let Ok(mut t) = tcgetattr(fd) {
        if on {
            t.local_modes.insert(LocalModes::ICANON);
        } else {
            t.local_modes.remove(LocalModes::ICANON);
        }
        let _ = tcsetattr(fd, OptionalActions::Now, &t);
    }
}

/// `FIONREAD` on `fd`: `(rc, errno, value)`.
fn fionread(fd: BorrowedFd<'_>) -> (libc::c_long, i32, i32) {
    let mut n: i32 = -1;
    let (rc, err) = ioctl(
        fd.as_raw_fd(),
        libc::FIONREAD as libc::c_ulong,
        &mut n as *mut i32 as usize,
    );
    (rc, err, n)
}

/// `FIONREAD` on `fd`, retried until it reads `want` or a short deadline
/// passes.
///
/// On the host a pty *master*'s write reaches the slave's line discipline
/// through a flip buffer that is processed asynchronously, so an immediate
/// query outruns it - measured: `slave=0` right after the write, `3` after
/// 30 ms (`.workbuddy/probe_ttyasync.c`). The guest is synchronous, so this
/// answers at once there and retries only on the host; a caller that wants a
/// *zero* must prove delivery another way (the echo, below).
fn fionread_want(fd: BorrowedFd<'_>, want: i32) -> i32 {
    let mut last = i32::MIN;
    for _ in 0..40 {
        let (rc, _, n) = fionread(fd);
        if rc == 0 {
            last = n;
            if n == want {
                return n;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    last
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

/// What the resize child saw, reported over a pipe: a fixed shape so the parent
/// reads one record rather than a stream it would have to parse.
#[derive(Clone, Copy, Default)]
struct ResizeReport {
    /// The child's process group, read back after `setsid`.
    pgid: i32,
    /// `tcgetattr` refused, or the handler would not install: the setup's own
    /// answer, so a failure is a claim rather than a hang.
    arm_rc: i32,
    /// `TIOCSCTTY`'s return code.
    ctty_rc: i32,
    /// The `TIOCSWINSZ` that should announce the change.
    resized: i32,
    /// How many times the handler ran.
    count: u32,
    /// The signal that ran it.
    signo: i32,
}

fn encode(report: ResizeReport) -> [u8; 24] {
    let mut out = [0u8; 24];
    for (i, word) in [
        report.pgid,
        report.arm_rc,
        report.ctty_rc,
        report.resized,
        report.count as i32,
        report.signo,
    ]
    .into_iter()
    .enumerate()
    {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn decode(bytes: &[u8; 24]) -> ResizeReport {
    let word = |i: usize| i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    ResizeReport {
        pgid: word(0),
        arm_rc: word(1),
        ctty_rc: word(2),
        resized: word(3),
        count: word(4) as u32,
        signo: word(5),
    }
}

/// The resize half, in a child of its own: a session leader whose controlling
/// terminal is the slave, so that a change to the window has a foreground group
/// to reach. Nothing is shared with the parent but the inherited descriptors,
/// and the child leaves through `exit_group` without printing.
fn resize_child(
    master: BorrowedFd<'_>,
    path: &std::ffi::CStr,
    rows: u16,
    cols: u16,
) -> ResizeReport {
    let (read_end, write_end) = rustix::pipe::pipe().expect("a pipe for the child's report");
    // SAFETY: the child writes one fixed-size record to a pipe it owns and
    // exits; it touches nothing the parent will read back.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        drop(read_end);
        // A session of its own, with no controlling terminal yet: what a getty
        // does before it opens the line.
        // SAFETY: `setsid` only fails for a process that is already a group
        // leader, and this one is a fresh child of the test.
        let mut report = ResizeReport::default();
        if unsafe { libc::setsid() } >= 0 {
            report.pgid = unsafe { libc::getpgrp() };
            // SAFETY: the child opens a terminal it will hold as its own; no
            // `O_NOCTTY` is exactly the point.
            if let Ok(slave) = open(path, OFlags::RDWR, Mode::empty()) {
                let (rc, _) = ioctl(
                    slave.as_fd().as_raw_fd(),
                    libc::TIOCSCTTY as libc::c_ulong,
                    0,
                );
                report.ctty_rc = rc as i32;
                let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
                act.sa_sigaction = on_winch
                    as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
                    as usize;
                act.sa_flags = libc::SA_SIGINFO;
                // SAFETY: a fully filled `sigaction` for a number the wrapper
                // serves, and no old record wanted.
                report.arm_rc =
                    unsafe { libc::sigaction(libc::SIGWINCH, &act, std::ptr::null_mut()) };
                report.resized = tcsetwinsize(
                    master,
                    Winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    },
                )
                .is_ok() as i32;
                // One more syscall: the frame the signal runs on is built on
                // the way out of the call that raised it.
                // SAFETY: `getpid` reads the caller's own process id.
                unsafe { libc::syscall(libc::SYS_getpid) };
                report.count = WINCHED.load(Ordering::Relaxed) as u32;
                report.signo = WINCH_SIG.load(Ordering::Relaxed);
            }
        }
        let bytes = encode(report);
        let _ = rustix::io::write(&write_end, &bytes);
        leave(0);
    }
    drop(write_end);
    let mut bytes = [0u8; 24];
    let mut filled = 0;
    while filled < bytes.len() {
        match rustix::io::read(&read_end, &mut bytes[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    // SAFETY: the child has reported and exited; its status is not the claim,
    // the report is.
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
    decode(&bytes)
}

fn main() {
    println!("t-pty");
    let mut r = Report::new();
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction = on_alarm as extern "C" fn(_, _, _) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves.
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) };

    let Some((master, number)) = mint() else {
        r.check(
            "/dev/ptmx opens and TIOCGPTN answers its pair's number",
            false,
        );
        r.finish();
    };

    // This kernel's minting order, not Linux's: see the note at the top.
    let past_the_consoles = number > 6;
    if !past_the_consoles {
        println!(
            "note: TIOCGPTN answered {number} - Linux's devpts numbers from 0, \
             where this kernel mints past its six virtual consoles"
        );
    }
    r.check(
        "the pair number is past the virtual consoles",
        past_the_consoles || mpos_user_tests::host_run(),
    );

    let path = slave_path(number);
    let slave = open(
        path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    );
    r.check("/dev/pts/N opens", slave.is_ok());
    let Some(slave) = slave.ok() else {
        r.finish();
    };

    // Both ends answer the terminal ioctls, as they do on Linux: a pty pair
    // is one terminal with two ends, so one termios belongs to the pair. A
    // supervisor holds only the master, and `isatty`/`TCGETS` on it is how it
    // decides whether it can do job control at all - `sudo` skipping its
    // `tcsetpgrp` on an `ENOTTY` master is a command left stopped forever.
    let master_termios = tcgetattr(&master);
    r.check("TCGETS on the master succeeds", master_termios.is_ok());
    let slave_termios = tcgetattr(&slave);
    r.check("TCGETS on the slave succeeds", slave_termios.is_ok());
    let one_terminal = match (&master_termios, &slave_termios) {
        (Ok(m), Ok(s)) => {
            m.local_modes == s.local_modes
                && m.special_codes[SpecialCodeIndex::VMIN]
                    == s.special_codes[SpecialCodeIndex::VMIN]
                && m.special_codes[SpecialCodeIndex::VTIME]
                    == s.special_codes[SpecialCodeIndex::VTIME]
        }
        _ => false,
    };
    if !one_terminal {
        // Diagnostic: the raw 36 bytes each end answers, word by word.
        for (name, fd) in [("master", &master), ("slave", &slave)] {
            let mut buf = [0xAAu8; 64];
            let (rc, err) = ioctl(
                fd.as_fd().as_raw_fd(),
                libc::TCGETS as libc::c_ulong,
                &mut buf as *mut [u8; 64] as usize,
            );
            println!(
                "note: TCGETS on the {name}: rc={rc} errno={err} words=[{:#010x}, {:#010x}, {:#010x}, {:#010x}] line={} cc={:?}",
                u32::from_le_bytes(buf[0..4].try_into().unwrap()),
                u32::from_le_bytes(buf[4..8].try_into().unwrap()),
                u32::from_le_bytes(buf[8..12].try_into().unwrap()),
                u32::from_le_bytes(buf[12..16].try_into().unwrap()),
                buf[16],
                &buf[17..36],
            );
        }
    }
    r.check("the two ends report one terminal", one_terminal);

    // The *window* is the pair's as well, and both ends read the same one:
    // this is how a terminal emulator tells the programs inside it how big
    // their window is - it sets the size on the master, they read it on the
    // slave - and a pair nobody has sized answers *nothing*: zero rows and zero
    // columns. That is the measured Linux answer (Emacs' own `getwinsize`
    // printing `0 columns 0 rows` is the report that pinned it,
    // `debbugs.gnu.org/24608`): a program answered `0` *knows* it was told
    // nothing and falls back the order its own documentation names, whereas a
    // guessed `25x80` takes that fallback away and hands it a window the real
    // one does not have.
    let unsized_pair = tcgetwinsize(&slave);
    r.check("TIOCGWINSZ on the slave succeeds", unsized_pair.is_ok());
    r.check(
        "an unsized pair answers no window",
        unsized_pair.is_ok_and(|ws| (ws.ws_row, ws.ws_col) == (0, 0)),
    );
    let sized = Winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    r.check(
        "TIOCSWINSZ on the master succeeds",
        tcsetwinsize(&master, sized).is_ok(),
    );
    let read_back = tcgetwinsize(&slave);
    r.check("TIOCGWINSZ after the set succeeds", read_back.is_ok());
    r.check(
        "the size set on the master is the slave's window",
        read_back.is_ok_and(|ws| (ws.ws_row, ws.ws_col) == (30, 100)),
    );

    // Master to slave: what is typed is what the line discipline hands the
    // reader, canonical mode and ICRNL included.
    let mut line = [0u8; 16];
    r.check(
        "the master write is taken whole",
        rustix::io::write(&master, b"hi\r") == Ok(3),
    );
    alarm(Some(400));
    let got = rustix::io::read(&slave, &mut line);
    alarm(None);
    r.check("the slave read is a whole line", got == Ok(3));
    r.check(
        "CR became LF on the way through",
        got == Ok(3) && &line[..3] == b"hi\n",
    );

    // Slave to master: what the slave prints is what the master reads. The
    // echo of the typed line is in the master's queue ahead of it, so that
    // goes down the drain first. It is four bytes and not three: the echo of
    // Enter is a line feed, and the terminal driver owes a *terminal* a
    // carriage return with it (`ONLCR`, on in the default termios) - a bare
    // line feed would leave the emulator's cursor where the line ended and
    // every line after it would start one column further right.
    let mut echo = [0u8; 16];
    alarm(Some(400));
    let got = rustix::io::read(&master, &mut echo);
    alarm(None);
    r.check("the typed line's echo reached the master", got == Ok(4));
    r.check(
        "the echo of Enter carries carriage return and line feed",
        got == Ok(4) && &echo[..4] == b"hi\r\n",
    );
    let mut printed = [0u8; 16];
    r.check(
        "the slave write is taken whole",
        rustix::io::write(&slave, b"out\n") == Ok(4),
    );
    alarm(Some(400));
    let got = rustix::io::read(&master, &mut printed);
    alarm(None);
    r.check("the master read sees the slave's bytes", got == Ok(5));
    r.check(
        "the printed newline arrives as carriage return and line feed",
        got == Ok(5) && &printed[..5] == b"out\r\n",
    );

    // SIGWINCH: a *change* to the window is announced. The signal goes to the
    // terminal's foreground group - which is a session leader's own group once
    // the slave is its controlling terminal, the getty's arrangement described
    // at the top of this file.
    let report = resize_child(master.as_fd(), path.as_c_str(), 40, 120);
    r.check(
        "the child is a session leader in a group of its own",
        report.pgid > 0,
    );
    r.check("a SIGWINCH handler installs", report.arm_rc == 0);
    r.check(
        "TIOCSCTTY made the slave the child's terminal",
        report.ctty_rc == 0,
    );
    r.check("a second TIOCSWINSZ succeeds", report.resized == 1);
    // The frame is built on the way out of the syscall that raised the signal,
    // so the child's extra syscall is what lets it count.
    r.check("the resize ran the handler exactly once", report.count == 1);
    r.check(
        "the signal that arrived is SIGWINCH",
        report.signo == libc::SIGWINCH,
    );

    // FIONREAD on a pty. A pair is one terminal and the request answers the
    // *input* queue of whichever end it is asked of, measured on the host
    // (`.workbuddy/probe_ttyinq.c`, Linux 7.2.9) - which corrects a claim this
    // tree carried that a terminal answers `0`. On the **slave** a *completed*
    // canonical line counts and the line still being typed does not, while in
    // raw mode every byte counts; on the **master** it is what the slave has
    // printed. Each case mints a fresh pair, because the line discipline's
    // buffer is exactly the state under test and the sections above have filled
    // it.
    let fresh_pair = || -> Option<(OwnedFd, OwnedFd)> {
        let (m, n) = mint()?;
        let s = open(
            slave_path(n).as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .ok()?;
        Some((m, s))
    };
    match fresh_pair() {
        Some((m, s)) => {
            set_canon(s.as_fd(), true);
            rustix::io::write(&m, b"hi").expect("type an incomplete canonical line");
            // Prove the line discipline has seen the bytes before believing the
            // slave's 0: the echo of the two characters lands on the master, and
            // it is the same delivery pass. Otherwise a 0 here would only mean
            // "not yet" - which is exactly the host's race.
            let echoed = fionread_want(m.as_fd(), 2);
            r.check(
                "the incomplete line's echo reaches the master (2)",
                echoed == 2,
            );
            let (_, _, n) = fionread(s.as_fd());
            r.check(
                "FIONREAD excludes the incomplete canonical line ('hi' answers 0)",
                n == 0,
            );
            rustix::io::write(&m, b"\n").expect("finish the line");
            r.check(
                "a completed canonical line counts ('hi\\n' answers 3)",
                fionread_want(s.as_fd(), 3) == 3,
            );
        }
        None => r.check("a fresh pty pair is minted for the canonical case", false),
    }
    match fresh_pair() {
        Some((m, s)) => {
            set_canon(s.as_fd(), true);
            rustix::io::write(&m, b"ab\ncd\n").expect("type two complete lines");
            r.check(
                "two completed canonical lines report 6",
                fionread_want(s.as_fd(), 6) == 6,
            );
        }
        None => r.check("a fresh pty pair is minted for the two-line case", false),
    }
    match fresh_pair() {
        Some((m, s)) => {
            set_canon(s.as_fd(), false);
            rustix::io::write(&m, b"hello").expect("type five raw bytes");
            r.check(
                "in raw mode every typed byte counts ('hello' answers 5)",
                fionread_want(s.as_fd(), 5) == 5,
            );
            let mut two = [0u8; 2];
            alarm(Some(400));
            let got = rustix::io::read(&s, &mut two);
            alarm(None);
            r.check("the two-byte raw read takes two", got == Ok(2));
            r.check(
                "after the read the remaining three count",
                fionread_want(s.as_fd(), 3) == 3,
            );
        }
        None => r.check("a fresh pty pair is minted for the raw case", false),
    }
    match fresh_pair() {
        Some((m, s)) => {
            rustix::io::write(&s, b"out").expect("print three bytes at the slave");
            r.check(
                "FIONREAD on the master reports what the slave printed (3)",
                fionread_want(m.as_fd(), 3) == 3,
            );
        }
        None => r.check("a fresh pty pair is minted for the master case", false),
    }

    drop(slave);
    drop(master);
    r.finish()
}
