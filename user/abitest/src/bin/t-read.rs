//! t-read - one test for read, which the strace uses to drain
//! /etc/ld.so.cache and /proc/self/maps three pages at a time.
//!
//! Pinned here on a real file: the first read hands back 1..n bytes, more
//! reads drain the rest, and the read at end-of-file answers 0 - the EOF
//! marker every musl reader loops on. Short reads are legal, so the count
//! is only bounded, never pinned. Also pinned: a zero-length read answers
//! 0 without touching the buffer, a descriptor that does not exist answers
//! EBADF, and a buffer with no readable pages behind it answers EFAULT
//! (a PROT_NONE mapping stands in for the unmapped page Linux would use).

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";

/// A fixed address far from the image base and the kernel's mmap bump.
const GUARD: usize = 0x5000_0000;

static mut BUF: [u8; 4096] = [0; 4096];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-read\n");
    let mut failed = false;

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the file to read opens", fd >= 0);

    if fd >= 0 {
        let mut total = 0usize;
        let mut eof = false;
        for _ in 0..8 {
            let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 4096) };
            let n = lin::read(fd as i32, buf);
            if n < 0 {
                lin::check_true(&mut failed, b"read answers a count, never an error", false);
                break;
            }
            total += n as usize;
            if n == 0 {
                eof = true;
                break;
            }
        }
        lin::check_true(&mut failed, b"the first read hands back bytes", total > 0);
        lin::check_true(&mut failed, b"reading past the end answers 0", eof);

        let empty: &mut [u8] = &mut [];
        let n = lin::read(fd as i32, empty);
        lin::check_eq(&mut failed, b"a zero-length read answers 0", n, 0);

        let _ = lin::close(fd as i32);
    }

    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
    let n = lin::read(99, buf);
    lin::check_eq(
        &mut failed,
        b"a descriptor that does not exist answers EBADF",
        n,
        lin::EBADF,
    );

    let p = lin::mmap(
        GUARD,
        4096,
        lin::PROT_NONE,
        lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
        -1,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the PROT_NONE guard page maps at the fixed address",
        p,
        GUARD as isize,
    );
    let buf = unsafe { core::slice::from_raw_parts_mut(GUARD as *mut u8, 16) };
    let n = lin::read(3, buf);
    lin::check_eq(
        &mut failed,
        b"a read into unreadable memory answers EFAULT",
        n,
        lin::EFAULT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-read: panicked\n")
}
