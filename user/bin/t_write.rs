//! t_write - one test for write, the one syscall that produces musl_hello's
//! output: `write(1, "Hello, world!\n", 14) = 14`.
//!
//! The test writes that exact payload to fd 1, a line to fd 2, and asks for
//! the exact byte counts back - a short or negative answer on a live
//! console fd is a bug. Then it writes to a descriptor that does not exist
//! and expects -EBADF, the error shape every musl wrapper keys on.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const HELLO: &[u8; 14] = b"Hello, world!\n";
const STDERR_LINE: &[u8; 21] = b"t_write: stderr line\n";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_write\n");
    let mut failed = false;

    let n = lin::write(1, HELLO);
    lin::check_eq(
        &mut failed,
        b"write(1, ...) reports every byte",
        n,
        HELLO.len() as isize,
    );

    let n = lin::write(2, STDERR_LINE);
    lin::check_eq(
        &mut failed,
        b"write(2, ...) reports every byte",
        n,
        STDERR_LINE.len() as isize,
    );

    let n = lin::write(-1, HELLO);
    lin::check_eq(
        &mut failed,
        b"a descriptor that does not exist answers EBADF",
        n,
        lin::EBADF,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_write: panicked\n")
}
