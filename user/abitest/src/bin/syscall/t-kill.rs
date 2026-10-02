//! t-kill - one test for kill, with signal 0 as the only signal this test
//! ever puts on the wire.
//!
//! The man page pins: kill(pid, 0) performs the permission and existence
//! checks but delivers nothing - the classic liveness probe - ESRCH for a
//! pid no process answers to, and EINVAL for a signal number outside the
//! range the kernel names — Linux's `_NSIG`, so `1..=64` including the
//! real-time signals (stage B). The range is checked before anything else, so
//! the refused numbers here (65, 999) are never delivered. No valid nonzero
//! signal is sent in this test: with no handler
//! machinery a delivered signal is fatal, and the test process
//! has work to finish.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-kill\n");
    let mut failed = false;
    let me = lin::getpid() as i32;

    // The existence probe: signal 0 delivers nothing, and we do exist.
    let r = lin::kill(me, 0);
    lin::check_eq(
        &mut failed,
        b"kill(self, 0) probes existence and succeeds",
        r,
        0,
    );

    // A pid past the whole task table names no process: ESRCH.
    let r = lin::kill(99999, 0);
    lin::check_eq(
        &mut failed,
        b"kill on a pid nobody owns is ESRCH",
        r,
        lin::ESRCH,
    );

    // The signal number is range-checked before anything else. The kernel names
    // `1..=64` (`_NSIG`) — the real-time range is valid from stage B on — so the
    // refused numbers are 65 (just past it) and 999 (far past it), not 32.
    let r = lin::kill(me, 65);
    lin::check_eq(
        &mut failed,
        b"kill(self, 65) past _NSIG is EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::kill(me, 999);
    lin::check_eq(&mut failed, b"kill(self, 999) is EINVAL", r, lin::EINVAL);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-kill: panicked\n")
}
