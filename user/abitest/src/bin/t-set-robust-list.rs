//! t-set-robust-list - one test for set_robust_list, which glibc calls at
//! startup (`set_robust_list(0x..., 24) = 0` in the strace) to register
//! its robust-futex head.
//!
//! Pinned here: the exact head size Linux demands - 24 bytes answers 0,
//! anything else answers EINVAL. The head's contents are never touched by
//! this syscall; only the registration is pinned.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut HEAD: lin::RobustListHead = lin::RobustListHead { next: 0 };

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-set-robust-list\n");
    let mut failed = false;

    let r = lin::set_robust_list(&raw const HEAD, 24);
    lin::check_eq(
        &mut failed,
        b"registering the 24-byte head succeeds",
        r,
        0,
    );

    let r = lin::set_robust_list(&raw const HEAD, 23);
    lin::check_eq(
        &mut failed,
        b"a wrong head size answers EINVAL",
        r,
        lin::EINVAL,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-set-robust-list: panicked\n")
}
