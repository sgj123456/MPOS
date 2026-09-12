//! t_access - one test for access, which the strace shows the dynamic
//! loader using to probe /etc/ld.so.preload (answered -1 ENOENT).
//!
//! Pinned here: an existing file is reachable for R_OK (answer 0) and a
//! path that does not exist answers ENOENT - which is exactly the loader's
//! expected outcome. The kernel currently answers 0 to everything; the
//! ENOENT check marks that gap. R/W/X bit distinctions are out of scope.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_access\n");
    let mut failed = false;

    let r = lin::access(&MOTD, lin::R_OK);
    lin::check_eq(
        &mut failed,
        b"an existing file is readable",
        r,
        0,
    );

    let r = lin::access(&NO_SUCH, lin::R_OK);
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
    lin::die(b"t_access: panicked\n")
}
