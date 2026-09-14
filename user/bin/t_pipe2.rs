//! t_pipe2 - one test for pipe2, the channel behind the shell's `|`.
//!
//! Pinned here: a flags = 0 call answers 0 and fills the pair with the two
//! lowest free descriptors (3 and 4), bytes written to fds[1] come back
//! from fds[0] whole, a flag word with bits beyond O_CLOEXEC answers
//! EINVAL, O_CLOEXEC alone is accepted, and once every write end is closed
//! the drained pipe reads 0 - the EOF every reader loop keys on.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut BUF: [u8; 16] = [0; 16];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_pipe2\n");
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
        b"flag bits beyond O_CLOEXEC answer EINVAL",
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

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_pipe2: panicked\n")
}
