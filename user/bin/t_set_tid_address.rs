//! t_set_tid_address - one test for set_tid_address, which musl_hello's
//! strace calls right after arch_prctl: musl's __init_tp registers the main
//! thread's clear-child-tid slot and uses the return value as the thread
//! id.
//!
//! The contract pinned here: the call answers with a live tid, equal to
//! getpid - the main thread's tid is its pid on Linux. The other half of
//! the contract, the kernel storing 0 into the slot when the thread exits,
//! is observable only through memory shared between parent and child, which
//! this test does not have; it is documented as out of scope.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The slot musl would register - left alone here, since its clear-on-exit
/// write is not observable from inside the process.
static mut SLOT: i64 = -1;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_set_tid_address\n");
    let mut failed = false;

    let tid = lin::set_tid_address(&raw mut SLOT);
    lin::check_true(
        &mut failed,
        b"set_tid_address returns a live tid",
        tid > 0,
    );
    lin::check_true(
        &mut failed,
        b"the tid is the process's own id",
        tid == lin::getpid(),
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_set_tid_address: panicked\n")
}
