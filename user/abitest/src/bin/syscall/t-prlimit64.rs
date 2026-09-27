//! t-prlimit64 - one test for prlimit64, which the strace shows twice
//! querying RLIMIT_STACK (`prlimit64(0, RLIMIT_STACK, NULL, {8192*1024,
//! RLIM64_INFINITY}) = 0`): Rust std reads the stack limit to size thread
//! stacks.
//!
//! Pinned here: a query with new=NULL fills the 16-byte struct rlimit64
//! with a usable soft limit and a hard limit that is at least as large.
//! The exact values are deliberately not pinned - only that the answer is
//! sane enough to compute with.
//!
//! Beyond the strace's call: RLIMIT_NOFILE is pinned to its exact soft
//! limit of 1024 - the number every descriptor-hoarding program branches
//! on - and the refusals are pinned too: a resource number past the table
//! is EINVAL, and old=NULL suppresses the query and answers 0.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut RL: core::mem::MaybeUninit<lin::Rlimit> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-prlimit64\n");
    let mut failed = false;

    let rl = &raw mut RL as *mut lin::Rlimit;
    let r = lin::prlimit64(0, lin::RLIMIT_STACK, core::ptr::null(), rl);
    lin::check_eq(&mut failed, b"querying RLIMIT_STACK succeeds", r, 0);
    unsafe {
        lin::check_true(&mut failed, b"the soft limit is usable", (*rl).rlim_cur > 0);
        lin::check_true(
            &mut failed,
            b"the hard limit is at least the soft one",
            (*rl).rlim_max >= (*rl).rlim_cur,
        );
    }

    // RLIMIT_NOFILE is a number programs compute with, not just sanity-check:
    // the soft limit is pinned to the classic 1024 exactly.
    let r = lin::prlimit64(0, lin::RLIMIT_NOFILE, core::ptr::null(), rl);
    lin::check_eq(&mut failed, b"querying RLIMIT_NOFILE succeeds", r, 0);
    unsafe {
        lin::check_eq(
            &mut failed,
            b"the RLIMIT_NOFILE soft limit is 1024",
            (*rl).rlim_cur as isize,
            1024,
        );
    }

    // A resource number past the table has no row to read: EINVAL.
    let r = lin::prlimit64(0, 99, core::ptr::null(), core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"an unknown resource of 99 is EINVAL",
        r,
        lin::EINVAL,
    );

    // old=NULL suppresses the query: the call is a valid no-op.
    let r = lin::prlimit64(
        0,
        lin::RLIMIT_STACK,
        core::ptr::null(),
        core::ptr::null_mut(),
    );
    lin::check_eq(
        &mut failed,
        b"a query with a null old pointer answers 0 and writes nothing",
        r,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-prlimit64: panicked\n")
}
