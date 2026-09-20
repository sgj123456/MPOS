//! t-gettid - one test for gettid, which the strace shows once
//! (`gettid() = 4172`): Rust std asks for its thread id.
//!
//! Pinned here: a live tid comes back, and for the main thread it equals
//! the process's own id - the main thread's tid is its pid on Linux, and
//! mpos already maps the number onto the same answer.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-gettid\n");
    let mut failed = false;

    let tid = lin::gettid();
    lin::check_true(&mut failed, b"gettid returns a live tid", tid > 0);
    lin::check_true(
        &mut failed,
        b"the main thread's tid is the process's pid",
        tid == lin::getpid(),
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-gettid: panicked\n")
}
