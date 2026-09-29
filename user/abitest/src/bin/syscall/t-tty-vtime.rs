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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PTMX: [u8; 10] = *b"/dev/ptmx\0";
static mut PATH: [u8; 20] = [0; 20];
static mut LINE: [u8; 16] = [0; 16];

/// `/dev/pts/<n>` for the number `TIOCGPTN` answered.
fn slave_path(n: usize) -> &'static [u8] {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PATH) };
    let path = lin::pts_path(n, buf);
    unsafe { core::slice::from_raw_parts(path.as_ptr(), path.len()) }
}

/// Put the terminal in the mode a full-screen program puts it in - no line
/// editing, no echo - with the `VMIN`/`VTIME` pair under test. Reports whether
/// the kernel took the settings, because a test that silently ran in canonical
/// mode would prove nothing.
fn set_raw(slave: i32, vmin: u8, vtime: u8) -> bool {
    let Some(mut tio) = lin::tcgetattr(slave) else {
        return false;
    };
    tio.lflag &= !(lin::ICANON | lin::ECHO);
    tio.cc[lin::VMIN] = vmin;
    tio.cc[lin::VTIME] = vtime;
    lin::tcsetattr(slave, &tio) == 0
        && lin::tcgetattr(slave).is_some_and(|back| {
            back.lflag & lin::ICANON == 0
                && back.cc[lin::VMIN] == vmin
                && back.cc[lin::VTIME] == vtime
        })
}

/// One timed read of `fd`, reporting the count and the elapsed milliseconds.
/// The buffer is cleared first so a failure line shows what actually landed.
fn timed_read(fd: i32) -> (isize, u64) {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
    for b in buf.iter_mut() {
        *b = 0;
    }
    let at = lin::monotonic_ms();
    let got = lin::read(fd, buf);
    (got, lin::monotonic_ms().wrapping_sub(at))
}

/// The line that timed read came back with.
fn line_text() -> &'static [u8] {
    unsafe {
        let buf = &*core::ptr::addr_of!(LINE);
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        &buf[..len]
    }
}

/// Write `bytes` at the master after `delay` ms, from a child: the read being
/// pinned here has to already be parked when the bytes land, which is the only
/// way a `VTIME` clock is observable at all.
fn feed_later(master: i32, delay: u64, bytes: &'static [u8]) -> i32 {
    let pid = lin::fork();
    if pid == 0 {
        lin::sleep_ms(delay);
        let _ = lin::write(master, bytes);
        lin::exit_group(0);
    }
    pid as i32
}

/// Report a check whose answer is a count *and* a duration, so a wrong number
/// and a right number at the wrong time both say so.
fn check_window(
    failed: &mut bool,
    name: &[u8],
    got: isize,
    want: isize,
    ms: u64,
    low: u64,
    high: u64,
) {
    if got == want && ms >= low && ms < high {
        lin::puts(b"ok:   ");
        lin::puts(name);
        lin::puts(b"\n");
    } else {
        *failed = true;
        lin::puts(b"FAIL: ");
        lin::puts(name);
        lin::puts(b" got=");
        lin::puts_isize(got);
        lin::puts(b" want=");
        lin::puts_isize(want);
        lin::puts(b" ms=");
        lin::puts_isize(ms as isize);
        lin::puts(b" expected in [");
        lin::puts_isize(low as isize);
        lin::puts(b",");
        lin::puts_isize(high as isize);
        lin::puts(b") text=");
        lin::puts(line_text());
        lin::puts(b"\n");
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-tty-vtime\n");
    let mut failed = false;

    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"a pty pair opens", master >= 0);
    let master = master as i32;
    let mut number: u32 = 0;
    if lin::ioctl(master, lin::TIOCGPTN, &raw mut number as usize) != 0 {
        lin::die(b"t-tty-vtime: TIOCGPTN failed\n");
    }
    let zero: i32 = 0;
    if lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize) != 0 {
        lin::die(b"t-tty-vtime: TIOCSPTLCK failed\n");
    }
    let slave = lin::openat(lin::AT_FDCWD, slave_path(number as usize), lin::O_RDWR, 0);
    lin::check_true(&mut failed, b"its slave opens", slave >= 0);
    let slave = slave as i32;

    // 1. `VMIN=0, VTIME=0` on an empty queue: 0 bytes, and no wait at all. The
    //    kernel must not read that 0 as an end of input - the terminal is
    //    standing right there - which is why the next read still works.
    lin::check_true(
        &mut failed,
        b"VMIN=0/VTIME=0 is accepted",
        set_raw(slave, 0, 0),
    );
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"a poll with nothing pending answers 0 at once",
        got,
        0,
        ms,
        0,
        50,
    );

    // 2. The same poll, with bytes already there: it takes them. A 0 answer
    //    consumed nothing, so nothing was lost between the two reads.
    if lin::write(master, b"abc") != 3 {
        lin::die(b"t-tty-vtime: master write failed\n");
    }
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"the same poll takes bytes that are already queued",
        got,
        3,
        ms,
        0,
        200,
    );
    lin::check_true(
        &mut failed,
        b"and they are the bytes written",
        line_text() == b"abc",
    );

    // 3. `VMIN=0, VTIME=1`: the clock runs from the read's own start, so an
    //    empty queue answers 0 about a decisecond later - never immediately,
    //    and never waiting for the next read to come round.
    lin::check_true(
        &mut failed,
        b"VMIN=0/VTIME=1 is accepted",
        set_raw(slave, 0, 1),
    );
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"a one-decisecond wait answers 0 in about a decisecond",
        got,
        0,
        ms,
        50,
        500,
    );

    // 4. `VMIN=0, VTIME=5` with three bytes landing at 200 ms: they come back
    //    as they arrive, because the clock is only an upper bound on an answer
    //    of its own. Measured as 100 ms for `VTIME=5`, never 500.
    lin::check_true(
        &mut failed,
        b"VMIN=0/VTIME=5 is accepted",
        set_raw(slave, 0, 5),
    );
    let kid = feed_later(master, 200, b"xyz");
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"bytes arriving mid-wait are delivered on arrival",
        got,
        3,
        ms,
        150,
        450,
    );
    lin::check_true(
        &mut failed,
        b"and are the bytes that landed",
        line_text() == b"xyz",
    );
    let mut status = 0i32;
    let _ = lin::wait4(kid as usize, &mut status, 0);

    // 5. The case that froze `vi`: one `ESC` and nothing after it. `VMIN=0`
    //    takes it on arrival, so the program can decide it was a lone escape
    //    key - and then its next poll answers 0 rather than sleeping forever.
    lin::check_true(
        &mut failed,
        b"VMIN=0/VTIME=1 is accepted again",
        set_raw(slave, 0, 1),
    );
    if lin::write(master, b"\x1b") != 1 {
        lin::die(b"t-tty-vtime: escape write failed\n");
    }
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"a lone escape is handed over at once",
        got,
        1,
        ms,
        0,
        300,
    );
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"and the poll after it answers 0, not forever",
        got,
        0,
        ms,
        50,
        500,
    );

    // 6. `VMIN=2, VTIME=5` with one byte at 200 ms: the read wants two, gets
    //    one, and hands it over one `VTIME` after it landed - the inter-byte
    //    clock, measured at 604 ms for a byte at 100 ms. A read that answered
    //    0 here, or waited for a second byte forever, is the wrong rule.
    lin::check_true(
        &mut failed,
        b"VMIN=2/VTIME=5 is accepted",
        set_raw(slave, 2, 5),
    );
    let kid = feed_later(master, 200, b"s");
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"one byte below VMIN comes back one VTIME after it lands",
        got,
        1,
        ms,
        500,
        1_300,
    );
    lin::check_true(
        &mut failed,
        b"and it is the byte that landed",
        line_text() == b"s",
    );
    let mut status = 0i32;
    let _ = lin::wait4(kid as usize, &mut status, 0);

    // 7. `VMIN=4, VTIME=5` with five bytes queued: `VMIN` is a floor, and the
    //    whole queue goes in one read - measured as 5 bytes for `VMIN=3`.
    lin::check_true(
        &mut failed,
        b"VMIN=4/VTIME=5 is accepted",
        set_raw(slave, 4, 5),
    );
    if lin::write(master, b"hello") != 5 {
        lin::die(b"t-tty-vtime: five-byte write failed\n");
    }
    let (got, ms) = timed_read(slave);
    check_window(
        &mut failed,
        b"a queue past VMIN is taken whole, with no wait",
        got,
        5,
        ms,
        0,
        300,
    );
    lin::check_true(&mut failed, b"and it is all of it", line_text() == b"hello");

    let _ = lin::close(slave);
    let _ = lin::close(master);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-tty-vtime: panicked\n")
}
