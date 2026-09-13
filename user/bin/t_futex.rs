//! t_futex - one test for futex, whose man page pins WAIT's
//! compare-and-block and WAKE's wake count. mpos's Linux surface is a
//! documented stub: WAIT answers 0 without ever blocking and WAKE answers 0
//! with nothing woken, and that stub shape is what the test pins - the man
//! page's EAGAIN on a value mismatch and its answers for the ops the stub
//! does not implement are never reached, so they are deliberately not
//! pinned.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The futex word: WAIT compares against it, WAKE would wake its waiters.
static mut WORD: i32 = 0;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_futex\n");
    let mut failed = false;

    // The stub answers 0 for WAIT without blocking, whatever the word holds.
    lin::check_eq(
        &mut failed,
        b"FUTEX_WAIT on the word answers 0",
        lin::futex(&raw mut WORD, lin::FUTEX_WAIT, 0),
        0,
    );

    // And 0 for WAKE, which would be the number woken on a real kernel.
    lin::check_eq(
        &mut failed,
        b"FUTEX_WAKE answers 0",
        lin::futex(&raw mut WORD, lin::FUTEX_WAKE, 1),
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_futex: panicked\n")
}
