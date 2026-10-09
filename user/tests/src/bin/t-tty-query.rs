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
//!
//! # The doors
//!
//! The pair is `t-pty`'s sequence (`/dev/ptmx` → `TIOCGPTN` → `TIOCSPTLCK(0)`
//! → `/dev/pts/N`), its requests taken from `libc`'s architecture table and
//! driven through `libc::syscall(SYS_ioctl)`. The mode is `rustix::termios`,
//! the only table that names `VMIN`/`VTIME`/`ICANON`/`ECHO` for Linux (see
//! `t-tty-vtime`). "Is anything waiting on this end" is `rustix::event::poll`
//! with a zero `Timespec`, which is `ppoll` on *both* machines - `poll` itself
//! has no arm64 number, the shape `t-sa-restart` names and `t-poll` pins.

use mpos_user_tests::Report;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
use rustix::time::Timespec;
use std::ffi::CString;
use std::time::Duration;

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

/// A fresh pty pair - `t-sa-restart`'s fixture, whose comment says why the
/// slave's path is read back from `TIOCGPTN` rather than written down.
fn fresh_pair() -> (OwnedFd, OwnedFd) {
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
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .unwrap_or_else(|e| panic!("opening {path:?}: {e}"));
    (master, slave)
}

/// Raw mode with a 100 ms clock, and a read-back that proves the kernel took
/// the settings: a test that silently ran in canonical mode would say nothing
/// about a reply nobody queued.
fn set_raw(fd: BorrowedFd<'_>) -> bool {
    let Ok(mut tio) = tcgetattr(fd) else {
        return false;
    };
    tio.local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO);
    tio.special_codes[SpecialCodeIndex::VMIN] = 0;
    tio.special_codes[SpecialCodeIndex::VTIME] = 1;
    tcsetattr(fd, OptionalActions::Now, &tio).is_ok()
        && tcgetattr(fd).is_ok_and(|back| {
            !back.local_modes.contains(LocalModes::ICANON)
                && back.special_codes[SpecialCodeIndex::VTIME] == 1
        })
}

/// Whether anything is waiting on this end at all.
fn waiting(fd: BorrowedFd<'_>) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let zero = Timespec::try_from(Duration::ZERO).expect("a representable zero");
    poll(&mut fds, Some(&zero)).unwrap_or(0) > 0 && fds[0].revents().contains(PollFlags::IN)
}

/// Spell the bytes a claim compared, so a failure line says what arrived rather
/// than only that it did not match.
fn spell(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        out.push_str(&format!("\\x{b:02x}"));
    }
    out
}

/// Read whatever is on this end now (the `VTIME` clock does the waiting) and
/// hand back what arrived.
fn read_now(fd: BorrowedFd<'_>) -> Vec<u8> {
    let mut buf = [0u8; 32];
    match rustix::io::read(fd, &mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(_) => Vec::new(),
    }
}

/// Nothing was queued at this end: no answer, and nothing else either.
fn check_silent(fd: BorrowedFd<'_>, name: &str, r: &mut Report) {
    let ready = waiting(fd);
    if ready {
        println!("note: unexpected bytes at {name}: {}", spell(&read_now(fd)));
    }
    r.check(name, !ready);
}

/// This end holds exactly these bytes.
fn check_holds(fd: BorrowedFd<'_>, want: &[u8], name: &str, r: &mut Report) {
    let got = read_now(fd);
    if got != want {
        println!("note: {name}: got {} want {}", spell(&got), spell(want));
    }
    r.check(name, got == want);
}

/// One sequence written at the program's end of a pty pair, checked in the two
/// directions Linux was measured in: it reaches the emulator whole, and the
/// kernel has answered none of it.
///
/// This is the contract for every terminal question this kernel used to answer
/// on someone else's behalf, and for every set that was never a question: the
/// same two readings, because to a line discipline on a wire there is no
/// difference.
fn travels(slave: BorrowedFd<'_>, master: BorrowedFd<'_>, seq: &[u8], name: &str, r: &mut Report) {
    r.check(
        "the sequence is written whole",
        rustix::io::write(slave, seq) == Ok(seq.len()),
    );
    // The program asked. Nothing may come back to it from this kernel.
    check_silent(slave, name, r);
    // And the question itself is still there to be asked - of the terminal that
    // owns the screen.
    check_holds(
        master,
        seq,
        "and the question reaches the emulator untouched",
        r,
    );
}

fn main() {
    println!("t-tty-query");
    let mut r = Report::new();

    let (master, slave) = fresh_pair();
    r.check("the slave is in raw mode", set_raw(slave.as_fd()));
    r.check("the master is in raw mode", set_raw(master.as_fd()));

    // --- the questions: forwarded, not answered ---
    //
    // The six shapes a shell, a theme probe and a capability query ask with.
    // Each is Linux's measured forwarding case, and each is a caller that would
    // have waited forever on a half-answer - `6n` most of all, because a shell
    // stops to read the reply before it reads another key.
    let questions: [(&[u8], &str); 8] = [
        (b"\x1b[6n", "CSI 6 n is the emulator's to answer"),
        (b"\x1b[?6n", "CSI ? 6 n is the emulator's to answer"),
        (b"\x1b[5n", "CSI 5 n is the emulator's to answer"),
        (b"\x1b[c", "CSI c is the emulator's to answer"),
        (b"\x1b[0c", "CSI 0 c is the emulator's to answer"),
        (b"\x1b[>c", "CSI > c is the emulator's to answer"),
        (b"\x1b]11;?\x07", "OSC 11 ; ? is the emulator's to answer"),
        (
            b"\x1bP1+q636f6c6f7273\x1b\\",
            "DCS 1 + q is the emulator's to answer",
        ),
    ];
    for (seq, label) in questions {
        travels(slave.as_fd(), master.as_fd(), seq, label, &mut r);
    }

    // --- the shapes that were never questions ---
    //
    // `CSI ? Ps c` is Linux's `csi_DEC` cursor-type *set*, spelled into the
    // `linux` terminfo's cursor capabilities, and vim writes `civis` and `cnorm`
    // on every command line it paints. Answering a set is what queued `9c9c` on
    // the prompt for the shell to try to run. Nothing is answered, and the set
    // reaches whoever draws.
    let sets: [(&[u8], &str); 3] = [
        (b"\x1b[?1c", "CSI ? 1 c is a cursor set, not a question"),
        (b"\x1b[?0c", "CSI ? 0 c is a cursor set, not a question"),
        (b"\x1b[?8c", "CSI ? 8 c is a cursor set, not a question"),
    ];
    for (seq, label) in sets {
        travels(slave.as_fd(), master.as_fd(), seq, label, &mut r);
    }

    // vim's command line as it actually writes it: hide the cursor, then set its
    // type, in one buffer.
    let civis: &[u8] = b"\x1b[?25l\x1b[?1c";
    r.check(
        "vim's civis is written",
        rustix::io::write(&slave, civis) == Ok(civis.len()),
    );
    check_silent(slave.as_fd(), "civis answers nothing", &mut r);
    check_holds(master.as_fd(), civis, "civis travels whole", &mut r);

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
        r.check(
            "the sequence is written whole",
            rustix::io::write(&slave, seq) == Ok(seq.len()),
        );
        check_silent(slave.as_fd(), "a set answers nothing", &mut r);
        check_holds(master.as_fd(), seq, "a set reaches the emulator", &mut r);
    }

    // Ordinary text is not a question either, and must not be held or answered.
    r.check(
        "the text is written whole",
        rustix::io::write(&slave, b"hello there") == Ok(11),
    );
    check_silent(slave.as_fd(), "text answers nothing", &mut r);
    check_holds(master.as_fd(), b"hello there", "text travels whole", &mut r);

    // --- typed keys are characters, cooked ---
    //
    // The same pair brought back to canonical mode: an arrow typed at a
    // canonical reader is *characters of its line*, delivered with it - the
    // answer `n_tty` gives, and the end of this kernel's private experiment
    // in recognising arrows and walking a history no editor was asked to
    // keep. A shell that edits reads raw and brings its own editor; the
    // discipline's job ends at ERASE, KILL and WERASE.
    let cooked = match tcgetattr(slave.as_fd()) {
        Ok(mut tio) => {
            tio.local_modes.insert(LocalModes::ICANON);
            tcsetattr(slave.as_fd(), OptionalActions::Now, &tio).is_ok()
        }
        Err(_) => false,
    };
    r.check("the slave goes canonical again", cooked);
    let typed: &[u8] = b"ab\x1b[A\r";
    r.check(
        "the typed keys are written",
        rustix::io::write(&master, typed) == Ok(typed.len()),
    );
    let mut buf = [0u8; 16];
    let got = rustix::io::read(slave.as_fd(), &mut buf).map_err(|e| e.raw_os_error());
    r.check("a canonical read takes the whole typed line", got == Ok(6));
    r.check(
        "and the arrow reaches the reader as the bytes typed",
        got == Ok(6) && &buf[..6] == b"ab\x1b[A\n",
    );

    drop(slave);
    drop(master);
    r.finish()
}
