//! true: succeed by doing nothing.

#![no_std]
#![no_main]

#[path = "../lib/sys.rs"]
mod sys;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    sys::exit(0);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    sys::abort()
}
