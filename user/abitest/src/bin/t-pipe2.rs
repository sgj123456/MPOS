//! t-pipe2 - one test for pipe2, the channel behind the shell's `|`.
//!
//! Pinned here: a flags = 0 call answers 0 and fills the pair with the two
//! lowest free descriptors (3 and 4), bytes written to fds[1] come back
//! from fds[0] whole, a flag word with bits beyond O_CLOEXEC|O_NONBLOCK
//! answers EINVAL, O_CLOEXEC alone is accepted, and once every write end is
//! closed the drained pipe reads 0 - the EOF every reader loop keys on.
//!
//! O_NONBLOCK is pinned with teeth: born through the flags word, an empty
//! read answers EAGAIN rather than parking the reader and a write to a
//! full pipe answers EAGAIN rather than parking the writer - which is the
//! birth state a self-pipe needs (sudo's signal channel was the caller
//! dying on its absence). `fcntl(F_SETFL)` grants the same state to a
//! plain pipe after the fact, and `F_GETFL`/`F_GETFD` report what the
//! flags word left on both ends.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut BUF: [u8; 16] = [0; 16];
/// A PIPE_BUF-sized record, for filling the pipe to a write's EAGAIN.
static mut BIG: [u8; 4096] = [0; 4096];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-pipe2\n");
    let mut failed = false;
    // Which number the lowest free descriptor is depends on what this process
    // inherited: a shell may hold one of its own. Ask, rather than assume 3.
    let base = lin::free_fd();

    let mut fds: [i32; 2] = [-1; 2];
    let r = lin::pipe2(&mut fds, 0);
    lin::check_eq(&mut failed, b"pipe2 with no flags succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"the read end is the lowest free fd",
        fds[0] as isize,
        base,
    );
    lin::check_eq(
        &mut failed,
        b"the write end is the next free fd",
        fds[1] as isize,
        base + 1,
    );

    let n = lin::write(fds[1], b"ping");
    lin::check_eq(&mut failed, b"the write end takes all 4 bytes", n, 4);
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
    let n = lin::read(fds[0], buf);
    lin::check_eq(&mut failed, b"the read end hands the 4 bytes back", n, 4);
    lin::check_true(
        &mut failed,
        b"the bytes that came back are the bytes written",
        n == 4 && buf[..4] == *b"ping",
    );
    let _ = lin::close(fds[0]);
    let _ = lin::close(fds[1]);

    let mut fds: [i32; 2] = [-1; 2];
    let r = lin::pipe2(&mut fds, 0xFFFF_FFFF);
    lin::check_eq(
        &mut failed,
        b"flag bits beyond O_CLOEXEC|O_NONBLOCK answer EINVAL",
        r,
        lin::EINVAL,
    );

    let mut fds: [i32; 2] = [-1; 2];
    let r = lin::pipe2(&mut fds, lin::O_CLOEXEC);
    lin::check_eq(&mut failed, b"pipe2 with O_CLOEXEC succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"the new pair again takes the two lowest free fds",
        fds[0] as isize == base && fds[1] as isize == base + 1,
    );

    let r = lin::close(fds[1]);
    lin::check_eq(&mut failed, b"closing the write end succeeds", r, 0);
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
    let n = lin::read(fds[0], buf);
    lin::check_eq(
        &mut failed,
        b"the drained pipe with no writers reads 0 (EOF)",
        n,
        0,
    );
    let _ = lin::close(fds[0]);

    // O_NONBLOCK: the birth state a self-pipe asks for. An empty read with
    // the writer still alive answers EAGAIN rather than parking the reader -
    // the call sudo's signal channel was dying on when the flag was refused.
    let mut fds: [i32; 2] = [-1; 2];
    let r = lin::pipe2(&mut fds, lin::O_NONBLOCK | lin::O_CLOEXEC);
    lin::check_eq(
        &mut failed,
        b"pipe2 with O_NONBLOCK|O_CLOEXEC succeeds",
        r,
        0,
    );
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
    let n = lin::read(fds[0], buf);
    lin::check_eq(
        &mut failed,
        b"an empty nonblocking read answers EAGAIN",
        n,
        lin::EAGAIN,
    );
    let fl = lin::fcntl(fds[0], lin::F_GETFL, 0);
    lin::check_true(
        &mut failed,
        b"F_GETFL reports the O_NONBLOCK the pipe was born with",
        fl >= 0 && fl as usize & lin::O_NONBLOCK != 0,
    );
    lin::check_true(
        &mut failed,
        b"O_CLOEXEC set FD_CLOEXEC on both ends",
        lin::fcntl(fds[0], lin::F_GETFD, 0) == lin::FD_CLOEXEC as isize
            && lin::fcntl(fds[1], lin::F_GETFD, 0) == lin::FD_CLOEXEC as isize,
    );
    // Data flows as ever: the flag changes what a *wait* is, not what a
    // read or a write takes.
    let n = lin::write(fds[1], b"pong");
    lin::check_eq(&mut failed, b"a nonblocking write with room succeeds", n, 4);
    let n = lin::read(fds[0], buf);
    lin::check_true(
        &mut failed,
        b"the bytes came back whole",
        n == 4 && buf[..4] == *b"pong",
    );
    // And the flag does not wedge the writer either: fill the pipe, and
    // once nothing more fits the write answers EAGAIN rather than waiting.
    let chunk = unsafe { core::slice::from_raw_parts(&raw const BIG as *const u8, 4096) };
    let mut landed = 0usize;
    let mut refused = false;
    for _ in 0..64 {
        let n = lin::write(fds[1], chunk);
        if n == lin::EAGAIN {
            refused = true;
            break;
        }
        landed += n as usize;
    }
    lin::check_true(
        &mut failed,
        b"a full pipe's nonblocking write answers EAGAIN",
        refused,
    );
    lin::check_true(
        &mut failed,
        b"the writes before it landed whole",
        landed >= 4096 && landed % 4096 == 0,
    );
    let _ = lin::close(fds[0]);
    let _ = lin::close(fds[1]);

    // `fcntl(F_SETFL)` grants the same state to a plain pipe after the
    // fact - the shape a program that cannot know about pipe2's flags word
    // uses, and the one a libc's `pipe()` plus `fcntl` idiom spells.
    let mut fds: [i32; 2] = [-1; 2];
    let r = lin::pipe2(&mut fds, 0);
    lin::check_eq(&mut failed, b"a plain pipe2(0) still succeeds", r, 0);
    let fl = lin::fcntl(fds[0], lin::F_GETFL, 0);
    lin::check_true(
        &mut failed,
        b"a plain pipe is born without O_NONBLOCK",
        fl >= 0 && fl as usize & lin::O_NONBLOCK == 0,
    );
    let r = lin::fcntl(fds[0], lin::F_SETFL, lin::O_NONBLOCK);
    lin::check_eq(&mut failed, b"F_SETFL sets O_NONBLOCK", r, 0);
    let n = lin::read(fds[0], buf);
    lin::check_eq(
        &mut failed,
        b"F_SETFL's O_NONBLOCK makes an empty read answer EAGAIN",
        n,
        lin::EAGAIN,
    );
    let _ = lin::close(fds[0]);
    let _ = lin::close(fds[1]);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-pipe2: panicked\n")
}
