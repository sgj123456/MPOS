//! t_readlink - one test for readlink, which the strace shows uutils using
//! to locate itself: `readlink("/proc/self/exe", ..., 256) = 29`.
//!
//! Pinned here: the Linux argument shape - NUL-terminated path, buffer,
//! capacity, no length word; a successful read answers the byte count and
//! the content is an absolute path without a NUL; a path that does not
//! exist answers ENOENT. On mpos, /proc/self/exe means the image path the
//! task was spawned from - the kernel knows it and must hand it back. The
//! exact content is deliberately not pinned beyond "absolute".

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static EXE: [u8; 15] = *b"/proc/self/exe\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static mut BUF: [u8; 256] = [0; 256];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_readlink\n");
    let mut failed = false;

    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 256) };
    let n = lin::readlink(&EXE, buf);
    lin::check_true(
        &mut failed,
        b"readlink(/proc/self/exe) answers the image path",
        n > 0 && n <= 256,
    );
    if n > 0 {
        lin::check_true(
            &mut failed,
            b"the target is an absolute path",
            buf[0] == b'/',
        );
    }

    let n = lin::readlink(&NO_SUCH, buf);
    lin::check_eq(
        &mut failed,
        b"a path that does not exist answers ENOENT",
        n,
        lin::ENOENT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_readlink: panicked\n")
}
