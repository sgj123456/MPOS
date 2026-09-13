//! t_fork - one test for fork, pinning the refusal.
//!
//! The man page expects fork to succeed: 0 back in the child, the child's
//! pid back in the parent, and two address-space copies running on. mpos
//! has no fork - duplicating an address space is the one process primitive
//! it deliberately does not offer, and coreutils are launched through
//! execve from the shell instead - so the syscall number answers ENOSYS by
//! design. This test documents and pins that answer: a libc or shell that
//! probes for fork gets a clean, honest failure it can fall back from,
//! not a half-working copy.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_fork\n");
    let mut failed = false;

    // Exactly ENOSYS: not 0 (which would lie about a child existing), not
    // a pid (which would orphan it) - the documented refusal.
    let r = lin::fork();
    lin::check_eq(
        &mut failed,
        b"fork is refused with ENOSYS",
        r,
        lin::ENOSYS,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_fork: panicked\n")
}
