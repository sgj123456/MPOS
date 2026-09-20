//! t-readlink - one test for readlink, which the strace shows uutils using
//! to locate itself: `readlink("/proc/self/exe", ..., 256) = 29`.
//!
//! Pinned here: the Linux argument shape - NUL-terminated path, buffer,
//! capacity, no length word; a successful read answers the byte count and
//! the content is an absolute path without a NUL; a zero-size buffer
//! answers EINVAL, a smaller buffer truncates silently to its size, an
//! existing path that is not a symlink answers EINVAL, and a missing one
//! ENOENT. On mpos, /proc/self/exe means the image path the task was
//! spawned from - the kernel knows it and must hand it back.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static EXE: [u8; 15] = *b"/proc/self/exe\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static MOTD: [u8; 10] = *b"/etc/motd\0";
static mut BUF: [u8; 256] = [0; 256];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-readlink\n");
    let mut failed = false;

    let full = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 256) };
    let n = lin::readlink(&EXE, full);
    lin::check_true(
        &mut failed,
        b"readlink(/proc/self/exe) answers the image path",
        n > 0 && n <= 256,
    );
    if n > 0 {
        lin::check_true(
            &mut failed,
            b"the target is an absolute path",
            full[0] == b'/',
        );
    }

    // A buffer one byte short truncates silently: Linux answers the
    // truncated length, not an error.
    if n > 1 {
        let shorter = &mut full[..(n as usize - 1)];
        let m = lin::readlink(&EXE, shorter);
        lin::check_eq(
            &mut failed,
            b"a smaller buffer truncates the answer",
            m,
            n - 1,
        );
    }

    // A zero-size buffer is a caller error, not a zero-byte read.
    let mut empty: [u8; 0] = [];
    let r = lin::readlink(&EXE, &mut empty);
    lin::check_eq(
        &mut failed,
        b"readlink with a zero-size buffer answers EINVAL",
        r,
        lin::EINVAL,
    );

    // An existing path that is not a symlink answers EINVAL.
    let r = lin::readlink(&MOTD, full);
    lin::check_eq(
        &mut failed,
        b"readlink on a non-symlink answers EINVAL",
        r,
        lin::EINVAL,
    );

    let r = lin::readlink(&NO_SUCH, full);
    lin::check_eq(
        &mut failed,
        b"a path that does not exist answers ENOENT",
        r,
        lin::ENOENT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-readlink: panicked\n")
}
