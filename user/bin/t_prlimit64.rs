//! t_prlimit64 - one test for prlimit64, which the strace shows twice
//! querying RLIMIT_STACK (`prlimit64(0, RLIMIT_STACK, NULL, {8192*1024,
//! RLIM64_INFINITY}) = 0`): Rust std reads the stack limit to size thread
//! stacks.
//!
//! Pinned here: a query with new=NULL fills the 16-byte struct rlimit64
//! with a usable soft limit and a hard limit that is at least as large.
//! The exact values are deliberately not pinned - only that the answer is
//! sane enough to compute with.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut RL: core::mem::MaybeUninit<lin::Rlimit> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_prlimit64\n");
    let mut failed = false;

    let rl = &raw mut RL as *mut lin::Rlimit;
    let r = lin::prlimit64(0, lin::RLIMIT_STACK, core::ptr::null(), rl);
    lin::check_eq(
        &mut failed,
        b"querying RLIMIT_STACK succeeds",
        r,
        0,
    );
    unsafe {
        lin::check_true(
            &mut failed,
            b"the soft limit is usable",
            (*rl).rlim_cur > 0,
        );
        lin::check_true(
            &mut failed,
            b"the hard limit is at least the soft one",
            (*rl).rlim_max >= (*rl).rlim_cur,
        );
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_prlimit64: panicked\n")
}
