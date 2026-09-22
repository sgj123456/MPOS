//! t-fionbio - `ioctl(fd, FIONBIO, &int)`: non-blocking mode set through
//! `ioctl` rather than `fcntl`.
//!
//! Rust's std uses this one and not `fcntl` on Linux. `read_output` - what
//! `Command::output()` uses to read a child's stdout and stderr at the same
//! time - begins by putting both pipes in non-blocking mode, and a kernel that
//! answers `ENOTTY` to a request it does not know therefore makes every Rust
//! `Command::output()` panic with "Not a tty". That is how `rustup-init` died.
//!
//! Pinned here: the call succeeds on a pipe, it sets the same `O_NONBLOCK`
//! that `fcntl(F_GETFL)` reports and `F_SETFL` writes, it actually bites - an
//! empty non-blocking pipe answers `EAGAIN` instead of parking the caller -
//! clearing it puts the descriptor back, and a descriptor that does not exist
//! is still `EBADF`.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fionbio\n");
    let mut failed = false;

    let mut fds = [0i32; 2];
    let r = lin::pipe2(&mut fds, 0);
    lin::check_eq(&mut failed, b"pipe2 makes a pipe", r, 0);
    if r != 0 {
        lin::done(true);
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // The argument is a pointer to an int, not the int itself.
    let mut on: i32 = 1;
    let r = lin::ioctl(read_fd, lin::FIONBIO, &mut on as *mut i32 as usize);
    lin::check_eq(&mut failed, b"FIONBIO on a pipe succeeds", r, 0);

    // FIONBIO and F_SETFL write the same bit, so F_GETFL has to see it - a
    // kernel that only stored the status word without telling the pipe would
    // pass the call and still block.
    let r = lin::fcntl(read_fd, lin::F_GETFL, 0);
    lin::check_true(
        &mut failed,
        b"F_GETFL reports O_NONBLOCK after FIONBIO",
        r as usize & lin::O_NONBLOCK != 0,
    );

    // And it bites: an empty non-blocking pipe is EAGAIN, not a wait.
    let mut buf = [0u8; 8];
    let r = lin::read(read_fd, &mut buf);
    lin::check_eq(
        &mut failed,
        b"an empty non-blocking pipe reads EAGAIN",
        r,
        lin::EAGAIN,
    );

    let r = lin::write(write_fd, b"x");
    lin::check_eq(&mut failed, b"a write to the pipe lands", r, 1);
    let r = lin::read(read_fd, &mut buf);
    lin::check_eq(&mut failed, b"and the byte reads back", r, 1);

    // Zero clears it, and F_GETFL agrees again.
    let mut off: i32 = 0;
    let r = lin::ioctl(read_fd, lin::FIONBIO, &mut off as *mut i32 as usize);
    lin::check_eq(&mut failed, b"FIONBIO with zero succeeds", r, 0);
    let r = lin::fcntl(read_fd, lin::F_GETFL, 0);
    lin::check_eq(
        &mut failed,
        b"F_GETFL no longer reports O_NONBLOCK",
        (r as usize & lin::O_NONBLOCK) as isize,
        0,
    );

    let r = lin::ioctl(999, lin::FIONBIO, &mut on as *mut i32 as usize);
    lin::check_eq(
        &mut failed,
        b"FIONBIO on a descriptor that is not open is EBADF",
        r,
        lin::EBADF,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fionbio: panicked\n")
}
