//! t_writev - one test for writev, the gather write behind every printf
//! that joins a prefix and a line without copying them together first.
//!
//! Pinned here: two iovecs to the console report exactly the sum of their
//! lengths - a short count on a live console fd is a bug - and a
//! descriptor that does not exist answers EBADF rather than a partial sum.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static HEAD: &[u8; 10] = b"t_writev: ";
static TAIL: &[u8; 9] = b"one line\n";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_writev\n");
    let mut failed = false;

    let iov = [
        lin::Iovec {
            base: HEAD.as_ptr() as *mut u8,
            len: HEAD.len(),
        },
        lin::Iovec {
            base: TAIL.as_ptr() as *mut u8,
            len: TAIL.len(),
        },
    ];

    let n = lin::writev(1, &iov);
    lin::check_eq(
        &mut failed,
        b"writev(1) reports exactly both lengths (10 + 9)",
        n,
        (HEAD.len() + TAIL.len()) as isize,
    );

    let n = lin::writev(99, &iov);
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
    lin::die(b"t_writev: panicked\n")
}
