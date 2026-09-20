//! t-rt_sigprocmask - one test for rt_sigprocmask, which a musl binary's
//! strace shows once: Rust std unblocking SIGRT_1 and SIGRT_2 at startup
//! (`rt_sigprocmask(SIG_UNBLOCK, [RT_1 RT_2], NULL, 8) = 0`).
//!
//! The test issues that exact call - the signal set as one 8-byte mask,
//! bit n holding signal n+1 - and then restores an empty mask, the other
//! call shape a libc cannot avoid around handler setup. Both must answer
//! 0. A query that answers 0 but leaves the caller's mask alone hands musl
//! garbage, so the written answer is part of the contract.
//!
//! Beyond the strace's two calls, the refusal set is pinned: an unknown
//! `how`, a `sigsetsize` other than the ABI's 8 (that one goes through a
//! raw `syscall` stub because the harness wrapper pins 8), a `set` that
//! cannot be read, and a query that must write an empty mask over a
//! sentinel-seeded out-word. `SIG_BLOCK` with a null `set` is allowed -
//! the set is optional.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::arch::asm;
use core::panic::PanicInfo;

/// SIGRT_1 = 34 lives in bit 33, SIGRT_2 = 35 in bit 34.
const RT_PAIR: u64 = (1u64 << 33) | (1u64 << 34);

/// `rt_sigprocmask` in its raw four-word shape, for the `sigsetsize`
/// variants the harness wrapper cannot spell: it hardcodes the 8 musl
/// always sends, and this test also owes an EINVAL for a 16.
fn rt_sigprocmask_raw(how: usize, set: usize, old: usize, sigsetsize: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") lin::nr::RT_SIGPROCMASK as isize => ret,
            in("rdi") how,
            in("rsi") set,
            in("rdx") old,
            in("r10") sigsetsize,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-rt_sigprocmask\n");
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

    // `how` is a two-value enum (0..=2); anything past it is refused.
    let r = lin::rt_sigprocmask(5, &set, core::ptr::null_mut());
    lin::check_eq(&mut failed, b"an unknown how of 5 is EINVAL", r, lin::EINVAL);

    // The ABI pins sigsetsize to 8 - the kernel validates the size before
    // it looks at anything else.
    let r = rt_sigprocmask_raw(
        lin::SIG_BLOCK as usize,
        &set as *const u64 as usize,
        0,
        16,
    );
    lin::check_eq(
        &mut failed,
        b"a sigsetsize of 16 is EINVAL",
        r,
        lin::EINVAL,
    );

    // A query has to write its answer: the seeded sentinel must be gone,
    // replaced by the empty mask a fresh process owes.
    let mut old: u64 = 0x0000_DEAD_BEEF;
    let r = lin::rt_sigprocmask(lin::SIG_SETMASK, core::ptr::null(), &mut old);
    lin::check_eq(
        &mut failed,
        b"querying the mask with a null set succeeds",
        r,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the query writes the empty mask over the sentinel",
        old as isize,
        0,
    );

    // The set is optional: changing nothing through SIG_BLOCK with a null
    // set is a valid, successful call.
    let r = lin::rt_sigprocmask(lin::SIG_BLOCK, core::ptr::null(), core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"SIG_BLOCK with a null set is allowed",
        r,
        0,
    );

    // A set pointer that names no mapping is EFAULT, not a crash. Address 1
    // would not do: these binaries are static-PIEs loaded from vaddr 0, so
    // their own first page is readable and the kernel is right to accept it.
    const NOWHERE: usize = 0x0000_0000_5000_0000;
    let r = lin::rt_sigprocmask(
        lin::SIG_BLOCK,
        NOWHERE as *const u64,
        core::ptr::null_mut(),
    );
    lin::check_eq(
        &mut failed,
        b"an unreadable set pointer is EFAULT",
        r,
        lin::EFAULT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-rt_sigprocmask: panicked\n")
}
