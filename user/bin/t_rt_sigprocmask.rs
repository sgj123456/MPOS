//! t_rt_sigprocmask - one test for rt_sigprocmask, which the musl_hello
//! strace shows once: Rust std unblocking SIGRT_1 and SIGRT_2 at startup
//! (`rt_sigprocmask(SIG_UNBLOCK, [RT_1 RT_2], NULL, 8) = 0`).
//!
//! The test issues that exact call - the signal set as one 8-byte mask,
//! bit n holding signal n+1 - and then restores an empty mask, the other
//! call shape a libc cannot avoid around handler setup. Both must answer
//! 0. Mask storage across calls is deliberately not probed: the strace
//! never reads a mask back.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// SIGRT_1 = 34 lives in bit 33, SIGRT_2 = 35 in bit 34.
const RT_PAIR: u64 = (1u64 << 33) | (1u64 << 34);

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_rt_sigprocmask\n");
    let mut failed = false;

    let set = RT_PAIR;
    let r = lin::rt_sigprocmask(lin::SIG_UNBLOCK, &set, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"unblocking SIGRT_1 and SIGRT_2 succeeds",
        r,
        0,
    );

    let empty: u64 = 0;
    let r = lin::rt_sigprocmask(lin::SIG_SETMASK, &empty, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"restoring an empty mask succeeds",
        r,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_rt_sigprocmask: panicked\n")
}
