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
//! programs inside it.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

static PTMX: [u8; 10] = *b"/dev/ptmx\0";

/// What the `SIGWINCH` handler saw. Shared across the frame boundary the way
/// `t-signal-delivery` shares its own - through atomics, since nothing on the
/// main line "touches" the handler and a plain static would let the compiler
/// cache the read.
static WINCHED: AtomicUsize = AtomicUsize::new(0);
static WINCH_SIG: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_winch(sig: i32, _info: *mut lin::SigInfo, _ctx: *mut u8) {
    WINCH_SIG.store(sig, Ordering::Relaxed);
    WINCHED.fetch_add(1, Ordering::Relaxed);
}

/// musl's `__restore_rt`, for a kernel with no libc: the handler's `ret` lands
/// here, and the only thing to do is hand the frame back to `rt_sigreturn`.
extern "C" fn winch_restorer() {
    let _ = lin::rt_sigreturn();
}

/// Ask for the resize signal, answering the syscall's own return code so the
/// caller can report a failure instead of dying in it.
fn arm_winch() -> isize {
    let act = lin::SigAction {
        handler: on_winch as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER,
        restorer: winch_restorer as extern "C" fn() as usize,
        mask: 0,
    };
    lin::rt_sigaction(lin::SIGWINCH, &act, core::ptr::null_mut())
}

static mut MASTER_TO_SLAVE: [u8; 8] = [0; 8];
static mut SLAVE_TO_MASTER: [u8; 8] = [0; 8];
static mut ECHO: [u8; 16] = [0; 16];
static mut PATH: [u8; 20] = [0; 20];
static mut SLAVE_LINE: [u8; 8] = [0; 8];

/// `/dev/pts/<n>` spelled into `PATH` from the number `TIOCGPTN` answered.
fn slave_path(mut n: usize) -> &'static [u8] {
    const PREFIX: [u8; 9] = *b"/dev/pts/";
    unsafe {
        let at = PREFIX.len();
        PATH[..at].copy_from_slice(&PREFIX);
        let mut digits = [0u8; 10];
        let mut len = 0;
        loop {
            digits[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
            if n == 0 {
                break;
            }
        }
        for (i, &d) in digits[..len].iter().rev().enumerate() {
            PATH[at + i] = d;
        }
        PATH[at + len] = 0;
        core::slice::from_raw_parts(core::ptr::addr_of!(PATH).cast(), at + len + 1)
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-pty\n");
    let mut failed = false;

    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/ptmx opens", master >= 0);
    let master = master as i32;

    // The number the slave will carry: at least past the six virtual
    // consoles, whose numbers are minted at boot.
    let mut n: u32 = 0;
    let r = lin::ioctl(master, lin::TIOCGPTN, &raw mut n as usize);
    lin::check_eq(&mut failed, b"TIOCGPTN answers the pair number", r, 0);
    lin::check_true(
        &mut failed,
        b"the pair number is past the virtual consoles",
        n > 6,
    );

    // `grantpt`/`unlockpt`: the lock clears, and a locked pair would refuse
    // its slave with EIO - the check that makes the sequence meaningful.
    let zero: i32 = 0;
    let r = lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize);
    lin::check_eq(&mut failed, b"TIOCSPTLCK(0) unlocks the pair", r, 0);

    let path = slave_path(n as usize);
    lin::puts(b"openat path: ");
    lin::puts(path);
    lin::puts(b"\n");
    let slave = lin::openat(lin::AT_FDCWD, path, lin::O_RDWR, 0);
    lin::puts(b"slave fd = ");
    lin::puts_isize(slave);
    lin::puts(b"\n");
    lin::check_true(&mut failed, b"/dev/pts/N opens", slave >= 0);
    let slave = slave as i32;

    // Both ends answer the terminal ioctls, as they do on Linux: a pty pair
    // is one terminal with two ends, so the termios belongs to the pair. A
    // supervisor holds only the master, and `isatty`/`TCGETS` on it is how it
    // decides whether it can do job control at all - `sudo` skipping its
    // `tcsetpgrp` on an `ENOTTY` master is a command left stopped forever.
    let mut termios = [0u8; 64];
    let r = lin::ioctl(master, lin::TCGETS, &raw mut termios as usize);
    lin::check_eq(&mut failed, b"TCGETS on the master succeeds", r, 0);
    let mut slave_termios = [0u8; 64];
    let r = lin::ioctl(slave, lin::TCGETS, &raw mut slave_termios as usize);
    lin::check_eq(&mut failed, b"TCGETS on the slave succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"the two ends report one terminal",
        termios == slave_termios,
    );

    // The *window* is the pair's as well, and both ends read the same one:
    // this is how a terminal emulator tells the programs inside it how big
    // their window is - it sets the size on the master, they read it on the
    // slave - and a pair nobody has sized answers the 25x80 stand-in. The
    // virtual consoles do not go through this: their size is the screen the
    // kernel console paints them on, so a full-screen program fills it.
    let mut ws = lin::Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0xFFFF,
        ws_ypixel: 0xFFFF,
    };
    let r = lin::ioctl(slave, lin::TIOCGWINSZ, &raw mut ws as usize);
    lin::check_eq(&mut failed, b"TIOCGWINSZ on the slave succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"an unsized pair answers the stand-in window",
        (ws.ws_row, ws.ws_col) == (25, 80),
    );
    ws.ws_row = 30;
    ws.ws_col = 100;
    let r = lin::ioctl(master, lin::TIOCSWINSZ, &raw const ws as usize);
    lin::check_eq(&mut failed, b"TIOCSWINSZ on the master succeeds", r, 0);
    let mut sized = lin::Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0xFFFF,
        ws_ypixel: 0xFFFF,
    };
    let r = lin::ioctl(slave, lin::TIOCGWINSZ, &raw mut sized as usize);
    lin::check_eq(&mut failed, b"TIOCGWINSZ after the set succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"the size set on the master is the slave's window",
        (sized.ws_row, sized.ws_col) == (30, 100),
    );

    // Master to slave: what is typed is what the line discipline hands the
    // reader, canonical mode and ICRNL included.
    let r = lin::write(master, b"hi\r");
    lin::check_eq(&mut failed, b"the master write is taken whole", r, 3);
    let got = lin::read(slave, unsafe { &mut *core::ptr::addr_of_mut!(SLAVE_LINE) });
    lin::check_eq(&mut failed, b"the slave read is a whole line", got, 3);
    unsafe {
        let line: [u8; 3] = core::ptr::read(core::ptr::addr_of!(SLAVE_LINE).cast());
        lin::check_true(
            &mut failed,
            b"CR became LF on the way through",
            &line == b"hi\n",
        );
    }

    // Slave to master: what the slave prints is what the master reads. The
    // echo of the typed line is in the master's queue ahead of it, so that
    // goes down the drain first. It is four bytes and not three: the echo of
    // Enter is a line feed, and the terminal driver owes a *terminal* a
    // carriage return with it (`ONLCR`, on in the default termios) - a bare
    // line feed would leave the emulator's cursor where the line ended and
    // every line after it would start one column further right.
    let got = lin::read(master, unsafe { &mut *core::ptr::addr_of_mut!(ECHO) });
    lin::check_eq(
        &mut failed,
        b"the typed line's echo reached the master",
        got,
        4,
    );
    unsafe {
        let echo: [u8; 4] = core::ptr::read(core::ptr::addr_of!(ECHO).cast());
        lin::check_true(
            &mut failed,
            b"the echo of Enter carries carriage return and line feed",
            &echo == b"hi\r\n",
        );
    }
    let r = lin::write(slave, b"out\n");
    lin::check_eq(&mut failed, b"the slave write is taken whole", r, 4);
    let got = lin::read(master, unsafe {
        &mut *core::ptr::addr_of_mut!(SLAVE_TO_MASTER)
    });
    lin::check_eq(
        &mut failed,
        b"the master read sees the slave's bytes",
        got,
        5,
    );
    unsafe {
        let line: [u8; 5] = core::ptr::read(core::ptr::addr_of!(SLAVE_TO_MASTER).cast());
        lin::check_true(
            &mut failed,
            b"the printed newline arrives as carriage return and line feed",
            &line == b"out\r\n",
        );
    }

    // SIGWINCH: a *change* to the window is announced, which is the one thing a
    // full-screen program cannot notice for itself - it is what makes `vim`
    // redraw when its terminal is resized, and `TIOCSWINSZ` above is what makes
    // the change. The signal goes to the terminal's foreground group, and the
    // read below is what makes the test's own group that: the reader of a tty is
    // the group a resize (and an interrupt) is aimed at - the rule a shell's job
    // control rests on, and the reason `t-ioctl` finds `SIGWINCH` harmless by
    // default.
    lin::check_true(
        &mut failed,
        b"the test runs in a process group a terminal can signal",
        lin::getpgrp() > 0,
    );
    let r = lin::write(master, b"z\r");
    lin::check_eq(&mut failed, b"the resize probe's bytes are taken", r, 2);
    let got = lin::read(slave, unsafe { &mut *core::ptr::addr_of_mut!(SLAVE_LINE) });
    lin::check_eq(&mut failed, b"the resize probe reads back", got, 2);

    let r = arm_winch();
    lin::check_eq(&mut failed, b"a SIGWINCH handler installs", r, 0);
    ws.ws_row = 40;
    ws.ws_col = 120;
    let r = lin::ioctl(master, lin::TIOCSWINSZ, &raw const ws as usize);
    lin::check_eq(&mut failed, b"a second TIOCSWINSZ succeeds", r, 0);
    // The frame is built on the way out of the syscall that raised the signal,
    // so one more syscall is enough to know the handler has run.
    let _ = lin::getpid();
    lin::check_eq(
        &mut failed,
        b"the resize ran the handler exactly once",
        WINCHED.load(Ordering::Relaxed) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"the signal that arrived is SIGWINCH",
        WINCH_SIG.load(Ordering::Relaxed) as isize,
        lin::SIGWINCH as isize,
    );

    let r = lin::close(slave);
    lin::check_eq(&mut failed, b"the slave closes", r, 0);
    let r = lin::close(master);
    lin::check_eq(&mut failed, b"the master closes", r, 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-pty: panicked\n")
}
