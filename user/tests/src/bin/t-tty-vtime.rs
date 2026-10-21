//! t-tty-vtime - the `VMIN`/`VTIME` read, which is how a full-screen program
//! tells a bare `ESC` from the start of an escape sequence, and how a poll
//! asks "is there anything?" without hanging.
//!
//! Every number here is measured on Linux, and the shape that matters is that
//! `VTIME` is an **inter-byte** clock rather than a deadline on the whole read:
//!
//! * `VMIN=0, VTIME=0` answers **0 bytes immediately**, whether or not bytes
//!   land a moment later - it is a poll, and a 0 there is not an end of input.
//! * `VMIN=0, VTIME=n` answers 0 one `n` deciseconds after the *read began*
//!   (102 ms at `n=1`), and answers the bytes themselves the moment they
//!   arrive (a byte at 100 ms came back at 100 ms, not at 300).
//! * `VMIN>0, VTIME=n` waits for `VMIN` bytes, and if fewer arrive, hands over
//!   what did arrive one `n` after the **last** of them - 1 byte at 100 ms came
//!   back at 604 ms with `VMIN=2/VTIME=5`, and a second byte that landed at
//!   900 ms was delivered 1300 ms, one `VTIME` after *that*.
//! * `VMIN` is a floor, not a count: 5 bytes arriving at once came back as 5
//!   with `VMIN=3`.
//!
//! This kernel got the first two wrong in the same direction, and the cost was
//! the freeze this test is named for: readiness was `out.len() >= VMIN.max(1)`,
//! so `VMIN == 0` was refused outright and a `0/1` read of an empty queue slept
//! forever instead of answering 0. busybox `vi` and `vim` set exactly that pair
//! to disambiguate `ESC`, so one escape key press wedged them with the terminal
//! still open. `VTIME` was read by nothing in the kernel at all.
//!
//! Driven through a pty pair rather than the console: the master is the
//! emulator's end, so the test can hand the slave's line discipline bytes at a
//! chosen moment - which is the whole point of a wait measured in deciseconds.
//! The pair is `t-pty`'s sequence (`/dev/ptmx` → `TIOCGPTN` → `TIOCSPTLCK(0)`
//! → `/dev/pts/N`, both ends `O_NOCTTY`), the two requests taken from `libc`'s
//! own architecture table and driven through `libc::syscall(SYS_ioctl)`.
//!
//! # The mode and the clock
//!
//! `VMIN` and `VTIME` are `c_cc` *indices* and `ICANON`/`ECHO` are `c_lflag`
//! bits, and this libc names none of them for Linux (measured: `VMIN`/`VTIME`/
//! `ICANON` appear under fuchsia, hurd, vxworks and the BSDs, and in no
//! `linux_like` file), so the mode is set through `rustix::termios` - which
//! names them all (`SpecialCodeIndex::VMIN`, `LocalModes::ICANON`) and hands
//! back a `Termios` whose layout is the kernel's. The read-back after every
//! `tcsetattr` is not decoration: a version of this program that silently ran
//! in canonical mode would prove nothing about a `VTIME` clock. The clock is
//! `std::time::Instant`, and it is a fixture rather than a claim - each claim
//! is an inequality between two moments the test itself placed.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, waitpid};
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
use std::ffi::CString;
use std::time::Instant;

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

/// Leave through `exit_group`: the child's inherited copy of the parent's
/// stdout buffer must not be flushed.
fn leave(code: i32) -> ! {
    // SAFETY: the door does not return; `_exit` is the maintained fallback.
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// Sleep out a whole number of milliseconds; only fixture children and the
/// setup between checks ever call it.
fn sleep_ms(ms: i64) {
    let req = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    // SAFETY: a `timespec` this call owns, and a null second pointer.
    unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
}

/// Put the terminal in the mode a full-screen program puts it in - no line
/// editing, no echo - with the `VMIN`/`VTIME` pair under test. Reports whether
/// the kernel took the settings, because a test that silently ran in canonical
/// mode would prove nothing.
fn set_mode(slave: BorrowedFd<'_>, vmin: u8, vtime: u8) -> bool {
    let Ok(mut tio) = tcgetattr(slave) else {
        return false;
    };
    tio.local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO);
    tio.special_codes[SpecialCodeIndex::VMIN] = vmin;
    tio.special_codes[SpecialCodeIndex::VTIME] = vtime;
    tcsetattr(slave, OptionalActions::Now, &tio).is_ok()
        && tcgetattr(slave).is_ok_and(|back| {
            !back.local_modes.contains(LocalModes::ICANON)
                && back.special_codes[SpecialCodeIndex::VMIN] == vmin
                && back.special_codes[SpecialCodeIndex::VTIME] == vtime
        })
}

/// One timed read of `fd`, reporting the count (or the refusal's number), the
/// elapsed milliseconds and the bytes themselves. The buffer is zeroed first so
/// a failure line shows what actually landed.
fn timed_read(fd: BorrowedFd<'_>) -> (Result<usize, i32>, u64, [u8; 16]) {
    let mut buf = [0u8; 16];
    let at = Instant::now();
    let got = rustix::io::read(fd, &mut buf).map_err(Errno::raw_os_error);
    let ms = at.elapsed().as_millis() as u64;
    (got, ms, buf)
}

/// Write `bytes` at the master after `delay` ms, from a child: the read being
/// pinned here has to already be parked when the bytes land, which is the only
/// way a `VTIME` clock is observable at all.
fn feed_later(master: BorrowedFd<'_>, delay_ms: i64, bytes: &'static [u8]) -> i32 {
    // SAFETY: the child sleeps, writes to a descriptor it owns, and exits; it
    // shares nothing mutable with the parent.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        sleep_ms(delay_ms);
        let _ = rustix::io::write(master, bytes);
        leave(0);
    }
    pid
}

fn reap(kid: i32) {
    let _ = waitpid(Pid::from_raw(kid), WaitOptions::empty());
}

/// The count and the interval a claim's answer has to land in: two numbers the
/// test itself placed (the bytes it wrote, and the moments it wrote them at).
#[derive(Clone, Copy)]
struct Window {
    want: usize,
    low: u64,
    high: u64,
}

/// Report a claim whose answer is a count *and* a duration, so a wrong number
/// and a right number at the wrong time both say so. The detail line goes out
/// only when the window is missed, so a green run prints claims and nothing
/// else.
fn check_window(
    r: &mut Report,
    name: &str,
    got: Result<usize, i32>,
    ms: u64,
    text: &[u8],
    window: Window,
) {
    let Window { want, low, high } = window;
    if got != Ok(want) || ms < low || ms >= high {
        println!(
            "note: {name}: got {got:?} want {want} in ms [{low},{high}) - ms {ms}, bytes {text:?}"
        );
    }
    r.check(name, got == Ok(want) && ms >= low && ms < high);
}

fn main() {
    println!("t-tty-vtime");
    let mut r = Report::new();

    let (master, slave) = fresh_pair();

    // 1. `VMIN=0, VTIME=0` on an empty queue: 0 bytes, and no wait at all. The
    //    kernel must not read that 0 as an end of input - the terminal is
    //    standing right there - which is why the next read still works.
    r.check("VMIN=0/VTIME=0 is accepted", set_mode(slave.as_fd(), 0, 0));
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "a poll with nothing pending answers 0 at once",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 0,
            low: 0,
            high: 50,
        },
    );

    // 2. The same poll, with bytes already there: it takes them. A 0 answer
    //    consumed nothing, so nothing was lost between the two reads.
    r.check(
        "three bytes are typed",
        rustix::io::write(&master, b"abc") == Ok(3),
    );
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "the same poll takes bytes that are already queued",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 3,
            low: 0,
            high: 200,
        },
    );
    r.check(
        "and they are the bytes written",
        got == Ok(3) && &buf[..3] == b"abc",
    );

    // 3. `VMIN=0, VTIME=1`: the clock runs from the read's own start, so an
    //    empty queue answers 0 about a decisecond later - never immediately,
    //    and never waiting for the next read to come round.
    r.check("VMIN=0/VTIME=1 is accepted", set_mode(slave.as_fd(), 0, 1));
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "a one-decisecond wait answers 0 in about a decisecond",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 0,
            low: 50,
            high: 500,
        },
    );

    // 4. `VMIN=0, VTIME=2` with three bytes landing at 50 ms: they come back
    //    as they arrive, because the clock is only an upper bound on an answer
    //    of its own. Measured on Linux as 100 ms for `VTIME=5`, never 500; the
    //    fixture is one decisecond of clock and 50 ms of delay, which is the
    //    same ordering with a smaller budget (`docs/notes/` §15.93).
    r.check("VMIN=0/VTIME=2 is accepted", set_mode(slave.as_fd(), 0, 2));
    let kid = feed_later(master.as_fd(), 50, b"xyz");
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "bytes arriving mid-wait are delivered on arrival",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 3,
            low: 20,
            high: 190,
        },
    );
    r.check(
        "and are the bytes that landed",
        got == Ok(3) && &buf[..3] == b"xyz",
    );
    reap(kid);

    // 5. The case that froze `vi`: one `ESC` and nothing after it. `VMIN=0`
    //    takes it on arrival, so the program can decide it was a lone escape
    //    key - and then its next poll answers 0 rather than sleeping forever.
    r.check(
        "VMIN=0/VTIME=1 is accepted again",
        set_mode(slave.as_fd(), 0, 1),
    );
    r.check(
        "the escape is typed",
        rustix::io::write(&master, b"\x1b") == Ok(1),
    );
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "a lone escape is handed over at once",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 1,
            low: 0,
            high: 300,
        },
    );
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "and the poll after it answers 0, not forever",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 0,
            low: 50,
            high: 500,
        },
    );

    // 6. `VMIN=2, VTIME=1` with one byte at 50 ms: the read wants two, gets
    //    one, and hands it over one `VTIME` after it landed - the inter-byte
    //    clock, measured at 604 ms for a byte at 100 ms with `VTIME=5`. The
    //    fixture keeps the shape at the shortest clock a decisecond allows: a
    //    byte at 50 ms comes back at about 150 ms. A read that answered 0 here,
    //    or waited for a second byte forever, is the wrong rule.
    r.check("VMIN=2/VTIME=1 is accepted", set_mode(slave.as_fd(), 2, 1));
    let kid = feed_later(master.as_fd(), 50, b"s");
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "one byte below VMIN comes back one VTIME after it lands",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 1,
            low: 100,
            high: 600,
        },
    );
    r.check(
        "and it is the byte that landed",
        got == Ok(1) && &buf[..1] == b"s",
    );
    reap(kid);

    // 7. `VMIN=4, VTIME=1` with five bytes queued: `VMIN` is a floor, and the
    //    whole queue goes in one read - measured as 5 bytes for `VMIN=3`.
    r.check("VMIN=4/VTIME=1 is accepted", set_mode(slave.as_fd(), 4, 1));
    r.check(
        "five bytes are typed",
        rustix::io::write(&master, b"hello") == Ok(5),
    );
    let (got, ms, buf) = timed_read(slave.as_fd());
    check_window(
        &mut r,
        "a queue past VMIN is taken whole, with no wait",
        got,
        ms,
        &buf[..got.unwrap_or(0)],
        Window {
            want: 5,
            low: 0,
            high: 300,
        },
    );
    r.check("and it is all of it", got == Ok(5) && &buf[..5] == b"hello");

    drop(slave);
    drop(master);
    r.finish()
}
