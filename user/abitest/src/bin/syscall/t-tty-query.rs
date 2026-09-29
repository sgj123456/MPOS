//! t-tty-query - what the console does when a program asks its terminal a
//! question, measured against Linux.
//!
//! There are two answers and the whole test is about picking the right one, which
//! is the question *who draws this screen* - not the question what the number
//! means.
//!
//! A pty pair is a wire. Measured on Linux, writing `ESC [ 6 n`, `ESC [ 5 n`,
//! `ESC [ c`, `ESC [ ? 6 n`, `OSC 11 ; ?` and `DCS 1 + q ... ST` to the slave of
//! a pair in raw mode with `ECHO` off delivered every one of them at the master,
//! byte for byte, and none of them was answered - in six of six runs. The
//! emulator holding the master has the cursor, the window and the capabilities,
//! and a line discipline that answers for it does two damage at once: the caller
//! gets a number that is not its screen's, and the answer the real terminal would
//! have sent is now a second one, arriving as typed input.
//!
//! That second half is not hypothetical, and this file used to pin the opposite
//! contract. The console here answered every question everywhere, and a shell on
//! a serial boot was told where its cursor was by a model of a screen nobody was
//! drawing: `fish` pads the line it is about to leave behind out to the width it
//! was told, and a width guessed at `80` in front of a window of any other width
//! never reaches the margin, never wraps, and the `CR` and `ESC [ K` that follow
//! then erase the line the command had just printed. Its `⏎` - the mark that
//! preserves a dangling line - was this kernel inventing a cursor.
//!
//! Where the kernel *is* the emulator - a virtual console whose grid the render
//! task has made, or a terminal with no wire behind it at all - it answers, and
//! keeps the question off the wire so nothing answers twice. That half cannot be
//! measured at a boot with no framebuffer, and is pinned by the host unit tests
//! over `LineDiscipline` and `term::Grid` instead: `vfs::tty::tests` holds the
//! answering cases, the window size a report is counted against, and the stream
//! corpus that keeps the report and the painted cursor the same pair.
//!
//! The shape that is *not* a question at all stays as it was, for the reason it
//! was found: `CSI ? Ps c` is Linux's `csi_DEC` cursor-type set, and the `linux`
//! terminfo spells its cursor capabilities with it - `civis=\E[?25l\E[?1c`,
//! `cnorm=\E[?25h\E[?0c`. Answering those sets is what left `9c9c` sitting on the
//! prompt after vim exited: two replies nobody had asked for, queued as input,
//! whose tails the shell read as text and tried to run. Forwarded, unanswered,
//! whole.
//!
//! Driven through a pty pair, because that is the one arrangement where both
//! halves can be *read*: the slave is the program's terminal, and the master is
//! the emulator's end. Each end is put where a full-screen program puts its tty -
//! no line editing, no echo, `VMIN=0/VTIME=1` - so that "nothing was answered" is
//! a measured answer rather than a hang.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PTMX: [u8; 10] = *b"/dev/ptmx\0";
static mut PATH: [u8; 20] = [0; 20];
/// Where a read lands. 32 is longer than the longest sequence pinned here, so a
/// short reply cannot be confused with a buffer that was not filled.
static mut SCRATCH: [u8; 32] = [0; 32];

/// `POLLIN`, as `poll` numbers it.
const POLLIN: i16 = 0x0001;

/// `/dev/pts/<n>` for the number `TIOCGPTN` answered.
fn slave_path(n: usize) -> &'static [u8] {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PATH) };
    let path = lin::pts_path(n, buf);
    unsafe { core::slice::from_raw_parts(path.as_ptr(), path.len()) }
}

/// Raw mode with a 100 ms clock, and a read-back that proves the kernel took
/// the settings: a test that silently ran in canonical mode would say nothing
/// about a reply nobody queued.
fn set_raw(fd: i32) -> bool {
    let Some(mut tio) = lin::tcgetattr(fd) else {
        return false;
    };
    tio.lflag &= !(lin::ICANON | lin::ECHO);
    tio.cc[lin::VMIN] = 0;
    tio.cc[lin::VTIME] = 1;
    lin::tcsetattr(fd, &tio) == 0
        && lin::tcgetattr(fd)
            .is_some_and(|back| back.lflag & lin::ICANON == 0 && back.cc[lin::VTIME] == 1)
}

/// Whether anything is waiting on this end at all.
fn waiting(fd: i32) -> bool {
    let mut fds = [lin::PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }];
    lin::poll(&mut fds, 0) > 0 && fds[0].revents & POLLIN != 0
}

/// Spell the bytes a test compared, so a failure in the guest log says what
/// arrived rather than only that it did not match.
fn spell(bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for &b in bytes {
        lin::puts(b"\\x");
        let hex = [DIGITS[(b >> 4) as usize], DIGITS[(b & 0xf) as usize]];
        lin::puts(&hex);
    }
}

/// Read whatever is on this end now (the `VTIME` clock does the waiting) into
/// `SCRATCH` and hand back what arrived.
fn read_now(fd: i32) -> &'static [u8] {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    let n = lin::read(fd, buf);
    if n <= 0 {
        return &[];
    }
    buf.get(..n as usize).unwrap_or(&[])
}

/// Nothing was queued at this end: no answer, and nothing else either.
fn check_silent(fd: i32, name: &[u8], failed: &mut bool) {
    if waiting(fd) {
        lin::puts(b"  unexpected bytes at ");
        lin::puts(name);
        lin::puts(b": ");
        spell(read_now(fd));
        lin::puts(b"\n");
        lin::check_true(failed, name, false);
    } else {
        lin::check_true(failed, name, true);
    }
}

/// This end holds exactly these bytes.
fn check_holds(fd: i32, want: &[u8], name: &[u8], failed: &mut bool) {
    let got = read_now(fd);
    if got != want {
        lin::puts(b"  ");
        lin::puts(name);
        lin::puts(b": got ");
        spell(got);
        lin::puts(b" want ");
        spell(want);
        lin::puts(b"\n");
    }
    lin::check_true(failed, name, got == want);
}

/// One sequence written at the program's end of a pty pair, checked in the two
/// directions Linux was measured in: it reaches the emulator whole, and the
/// kernel has answered none of it.
///
/// This is the contract for every terminal question this kernel used to answer on
/// someone else's behalf, and for every set that was never a question: the same
/// two readings, because to a line discipline on a wire there is no difference.
fn travels(slave: i32, master: i32, seq: &[u8], name: &[u8], failed: &mut bool) {
    lin::check_eq(
        failed,
        b"the sequence is written whole",
        lin::write(slave, seq),
        seq.len() as isize,
    );
    // The program asked. Nothing may come back to it from this kernel.
    check_silent(slave, name, failed);
    // And the question itself is still there to be asked - of the terminal that
    // owns the screen.
    check_holds(
        master,
        seq,
        b"and the question reaches the emulator untouched",
        failed,
    );
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-tty-query\n");
    let mut failed = false;

    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/ptmx opens", master >= 0);
    let master = master as i32;

    let mut n: u32 = 0;
    let r = lin::ioctl(master, lin::TIOCGPTN, &raw mut n as usize);
    lin::check_eq(&mut failed, b"TIOCGPTN answers the pair number", r, 0);
    let zero: i32 = 0;
    let r = lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize);
    lin::check_eq(&mut failed, b"TIOCSPTLCK(0) unlocks the pair", r, 0);

    let slave = lin::openat(lin::AT_FDCWD, slave_path(n as usize), lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"/dev/pts/N opens", slave >= 0);
    let slave = slave as i32;

    lin::check_true(&mut failed, b"the slave is in raw mode", set_raw(slave));
    lin::check_true(&mut failed, b"the master is in raw mode", set_raw(master));

    // --- the questions: forwarded, not answered ---
    //
    // The six shapes a shell, a theme probe and a capability query ask with. Each
    // is Linux's measured forwarding case, and each is a caller that would have
    // waited forever on a half-answer - `6n` most of all, because a shell stops to
    // read the reply before it reads another key.

    let questions: [(&[u8], &[u8]); 8] = [
        (b"\x1b[6n", b"CSI 6 n is the emulator's to answer"),
        (b"\x1b[?6n", b"CSI ? 6 n is the emulator's to answer"),
        (b"\x1b[5n", b"CSI 5 n is the emulator's to answer"),
        (b"\x1b[c", b"CSI c is the emulator's to answer"),
        (b"\x1b[0c", b"CSI 0 c is the emulator's to answer"),
        (b"\x1b[>c", b"CSI > c is the emulator's to answer"),
        (b"\x1b]11;?\x07", b"OSC 11 ; ? is the emulator's to answer"),
        (
            b"\x1bP1+q636f6c6f7273\x1b\\",
            b"DCS 1 + q is the emulator's to answer",
        ),
    ];
    for (seq, label) in questions {
        travels(slave, master, seq, label, &mut failed);
    }

    // --- the shapes that were never questions ---
    //
    // `CSI ? Ps c` is Linux's `csi_DEC` cursor-type *set*, spelled into the
    // `linux` terminfo's cursor capabilities, and vim writes `civis` and `cnorm`
    // on every command line it paints. Answering a set is what queued `9c9c` on
    // the prompt for the shell to try to run. Nothing is answered, and the set
    // reaches whoever draws.
    let sets: [(&[u8], &[u8]); 3] = [
        (b"\x1b[?1c", b"CSI ? 1 c is a cursor set, not a question"),
        (b"\x1b[?0c", b"CSI ? 0 c is a cursor set, not a question"),
        (b"\x1b[?8c", b"CSI ? 8 c is a cursor set, not a question"),
    ];
    for (seq, label) in sets {
        travels(slave, master, seq, label, &mut failed);
    }

    // vim's command line as it actually writes it: hide the cursor, then set its
    // type, in one buffer.
    let civis: &[u8] = b"\x1b[?25l\x1b[?1c";
    lin::check_eq(
        &mut failed,
        b"vim's civis is written",
        lin::write(slave, civis),
        civis.len() as isize,
    );
    check_silent(slave, b"civis answers nothing", &mut failed);
    check_holds(master, civis, b"civis travels whole", &mut failed);

    // A colour set, a scroll region, a cursor move: drawn with, never answered.
    let drawn: &[&[u8]] = &[
        b"\x1b[32m",
        b"\x1b[4;10r",
        b"\x1b[3;5H",
        b"\x1b[?1049h",
        b"\x1b[s",
        b"\x1b[u",
    ];
    for &seq in drawn {
        check_eq_len(slave, seq, &mut failed);
        check_silent(slave, b"a set answers nothing", &mut failed);
        check_holds(master, seq, b"a set reaches the emulator", &mut failed);
    }

    // Ordinary text is not a question either, and must not be held or answered.
    check_eq_len(slave, b"hello there", &mut failed);
    check_silent(slave, b"text answers nothing", &mut failed);
    check_holds(master, b"hello there", b"text travels whole", &mut failed);

    // --- typed keys are characters, cooked ---
    //
    // The same pair brought back to canonical mode: an arrow typed at a
    // canonical reader is *characters of its line*, delivered with it - the
    // answer `n_tty` gives, and the end of this kernel's private experiment
    // in recognising arrows and walking a history no editor was asked to
    // keep. A shell that edits reads raw and brings its own editor; the
    // discipline's job ends at ERASE, KILL and WERASE.
    let cooked = match lin::tcgetattr(slave) {
        Some(mut tio) => {
            tio.lflag |= lin::ICANON;
            lin::tcsetattr(slave, &tio) == 0
        }
        None => false,
    };
    lin::check_true(&mut failed, b"the slave goes canonical again", cooked);
    let typed: &[u8] = b"ab\x1b[A\r";
    check_eq_len(master, typed, &mut failed);
    let mut buf = [0u8; 16];
    let got = lin::read(slave, &mut buf);
    lin::check_eq(
        &mut failed,
        b"a canonical read takes the whole typed line",
        got,
        6,
    );
    lin::check_true(
        &mut failed,
        b"and the arrow reaches the reader as the bytes typed",
        buf[..6] == *b"ab\x1b[A\n",
    );

    let _ = lin::close(slave);
    let _ = lin::close(master);
    lin::done(failed)
}

/// The write itself: one check, so a short write is a named failure and not a
/// mystery about the two reads that follow it.
fn check_eq_len(fd: i32, seq: &[u8], failed: &mut bool) {
    lin::check_eq(
        failed,
        b"the sequence is written whole",
        lin::write(fd, seq),
        seq.len() as isize,
    );
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-tty-query: panicked\n")
}
