//! t-rt-sigaction - one test for rt_sigaction, of which a musl binary's
//! strace shows four calls on the way up:
//!
//! - a query of the untouched SIGSEGV and SIGBUS dispositions
//!   (`rt_sigaction(SIGSEGV, NULL, {SIG_DFL}, 8)`)
//! - SIGPIPE set to SIG_IGN, which Rust std does at startup and panics
//!   with "cannot ignore SIGPIPE" if it fails - so a 0 here is load-bearing
//! - SIGSEGV and SIGBUS armed with `SA_SIGINFO|SA_ONSTACK` on top of the
//!   alt stack sigaltstack installed
//!
//! Every call must answer 0. A query must also actually write the old
//! disposition: the test seeds the out-struct with sentinels and demands
//! SIG_DFL back, because a query that answers 0 but writes nothing hands
//! musl uninitialized memory. Re-reading back what an install stored is
//! deliberately not asserted - the strace never does it, and hello world
//! never runs a handler.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const SEED: u64 = 0x5a5a_5a5a_5a5a_5a5a;

extern "C" fn restorer() {}
extern "C" fn segv_handler() {}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-rt-sigaction\n");
    let mut failed = false;
    let restorer_addr = restorer as extern "C" fn() as usize;
    let handler_addr = segv_handler as extern "C" fn() as usize;

    // The fresh-process query: the sentinels must all be gone.
    let mut old = lin::SigAction { handler: SEED as usize, flags: SEED as usize, restorer: SEED as usize, mask: SEED };
    let r = lin::rt_sigaction(lin::SIGSEGV, core::ptr::null(), &mut old);
    lin::check_eq(
        &mut failed,
        b"querying the fresh SIGSEGV disposition succeeds",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the query reports SIG_DFL with clear flags and mask",
        old.handler == lin::SIG_DFL && old.flags == 0 && old.mask == 0,
    );

    // Rust std ignores SIGPIPE before main ever runs.
    let act = lin::SigAction {
        handler: lin::SIG_IGN,
        flags: lin::SA_RESTORER,
        restorer: restorer_addr,
        mask: 0,
    };
    let r = lin::rt_sigaction(lin::SIGPIPE, &act, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"installing SIG_IGN on SIGPIPE succeeds",
        r,
        0,
    );

    // Rust std arms SIGSEGV and SIGBUS with SA_ONSTACK for its stack-overflow
    // guard - the flag combination the strace shows.
    let act = lin::SigAction {
        handler: handler_addr,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER | lin::SA_ONSTACK | lin::SA_RESTART,
        restorer: restorer_addr,
        mask: 0,
    };
    let r = lin::rt_sigaction(lin::SIGSEGV, &act, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"installing the SIGSEGV handler succeeds",
        r,
        0,
    );
    let r = lin::rt_sigaction(lin::SIGBUS, &act, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"installing the SIGBUS handler succeeds",
        r,
        0,
    );

    // The refusal set the man page fixes: signal 0 is not a signal,
    // SIGKILL/SIGSTOP cannot be given a disposition by anyone, and 32 is
    // the first real-time number past the 0..=31 this kernel can name.
    let r = lin::rt_sigaction(0, core::ptr::null(), core::ptr::null_mut());
    lin::check_eq(&mut failed, b"rt_sigaction on signal 0 is EINVAL", r, lin::EINVAL);
    let r = lin::rt_sigaction(
        lin::SIGKILL,
        core::ptr::null(),
        core::ptr::null_mut(),
    );
    lin::check_eq(
        &mut failed,
        b"rt_sigaction on SIGKILL is EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::rt_sigaction(
        lin::SIGSTOP,
        core::ptr::null(),
        core::ptr::null_mut(),
    );
    lin::check_eq(
        &mut failed,
        b"rt_sigaction on SIGSTOP is EINVAL",
        r,
        lin::EINVAL,
    );
    let r = lin::rt_sigaction(32, core::ptr::null(), core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"rt_sigaction on the first unnumbered RT signal is EINVAL",
        r,
        lin::EINVAL,
    );

    // A pointer that names no mapping must be EFAULT, not a crash: the
    // kernel validates before it reads. It has to be an address that is
    // really unmapped, which 1 is *not* in this suite - every one of these
    // binaries is linked as a static-PIE whose first LOAD starts at vaddr 0,
    // so the low pages are the program's own text and are perfectly readable.
    // The gap above the image's base and below the stack is not.
    const NOWHERE: usize = 0x0000_0000_5000_0000;
    let r = lin::rt_sigaction(
        lin::SIGPIPE,
        NOWHERE as *const lin::SigAction,
        core::ptr::null_mut(),
    );
    lin::check_eq(
        &mut failed,
        b"an unreadable act pointer is EFAULT",
        r,
        lin::EFAULT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-rt-sigaction: panicked\n")
}
