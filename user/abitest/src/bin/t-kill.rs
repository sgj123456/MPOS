//! t-kill - one test for kill, with signal 0 as the only signal this test
//! ever puts on the wire.
//!
//! The man page pins: kill(pid, 0) performs the permission and existence
//! checks but delivers nothing - the classic liveness probe - ESRCH for a
//! pid no process answers to, and EINVAL for a signal number outside the
//! set the kernel can name. mpos has 32 signal numbers, 0..=31, so 32 and
//! anything above are refused. No nonzero signal is ever sent here: with
//! no handler machinery a delivered signal is fatal, and the test process
//! has work to finish.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
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

    // The signal number is range-checked before anything else: 32 is one
    // past the 0..=31 the kernel can name, and 999 is far past it.
    let r = lin::kill(me, 32);
    lin::check_eq(&mut failed, b"kill(self, 32) is EINVAL", r, lin::EINVAL);
    let r = lin::kill(me, 999);
    lin::check_eq(&mut failed, b"kill(self, 999) is EINVAL", r, lin::EINVAL);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-kill: panicked\n")
}
