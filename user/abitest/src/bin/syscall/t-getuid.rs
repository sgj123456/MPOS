//! t-getuid - one test for the credential quartet: getuid, getgid,
//! geteuid, and getegid.
//!
//! The man page pins: getuid and geteuid return the real and effective
//! user id, getgid and getegid the group ids - always successful, no
//! errno. A fresh process on this kernel starts as root, so all four
//! answers are exactly 0 here; pinned exactly, not just "some id", so a
//! caller branching on uid == 0 to decide whether to try something
//! privileged gets a stable answer at startup - the `t_posix` test is the
//! one that pins what happens after a `setuid` dance moves them.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-getuid\n");
    let mut failed = false;

    lin::check_eq(&mut failed, b"getuid is 0", lin::getuid(), 0);
    lin::check_eq(&mut failed, b"getgid is 0", lin::getgid(), 0);
    lin::check_eq(&mut failed, b"geteuid is 0", lin::geteuid(), 0);
    lin::check_eq(&mut failed, b"getegid is 0", lin::getegid(), 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-getuid: panicked\n")
}
