//! t_getuid - one test for the credential quartet: getuid, getgid,
//! geteuid, and getegid.
//!
//! The man page pins: getuid and geteuid return the real and effective
//! user id, getgid and getegid the group ids - always successful, no
//! errno. mpos is a one-user kernel: root owns everything, so all four
//! answers are exactly 0, the uid every permission check in the kernel
//! treats as "may proceed". Pinned exactly, not just "some id": a caller
//! branching on uid == 0 to decide whether to try something privileged
//! gets a stable answer on both sides.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_getuid\n");
    let mut failed = false;

    lin::check_eq(&mut failed, b"getuid is 0", lin::getuid(), 0);
    lin::check_eq(&mut failed, b"getgid is 0", lin::getgid(), 0);
    lin::check_eq(&mut failed, b"geteuid is 0", lin::geteuid(), 0);
    lin::check_eq(&mut failed, b"getegid is 0", lin::getegid(), 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_getuid: panicked\n")
}
