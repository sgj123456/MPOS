//! t-write - one test for write, the one syscall that produces a musl binary's
//! output: `write(1, "Hello, world!\n", 14) = 14`.
//!
//! The test writes that exact payload to fd 1, a line to fd 2, and asks for
//! the exact byte counts back - a short or negative answer on a live
//! console fd is a bug. Then it writes to a descriptor that does not exist
//! and expects -EBADF, the error shape every musl wrapper keys on. Also
//! pinned: a zero-length write answers 0, and a source buffer with no
//! readable pages behind it answers EFAULT (a PROT_NONE mapping stands in
//! for the unmapped page Linux would use).

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const HELLO: &[u8; 14] = b"Hello, world!\n";
const STDERR_LINE: &[u8; 21] = b"t-write: stderr line\n";

/// A fixed address far from the image base and the kernel's mmap bump.
const GUARD: usize = 0x5000_1000;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-write\n");
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

    let n = lin::write(1, &[]);
    lin::check_eq(&mut failed, b"a zero-length write answers 0", n, 0);

    let p = lin::mmap(
        GUARD,
        4096,
        lin::PROT_NONE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(&mut failed, b"the PROT_NONE guard page maps at the fixed address", p, GUARD as isize);
    let src = unsafe { core::slice::from_raw_parts(GUARD as *const u8, 16) };
    let n = lin::write(1, src);
    lin::check_eq(
        &mut failed,
        b"a write from unreadable memory answers EFAULT",
        n,
        lin::EFAULT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-write: panicked\n")
}
