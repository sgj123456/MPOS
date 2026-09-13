//! t_sigaltstack - one test for sigaltstack, which a musl strace
//! uses twice: a query of the fresh process (`sigaltstack(NULL,
//! {SS_DISABLE})`) and the install of the 8 KiB stack Rust std reserved
//! with mmap for its SIGSEGV/SIGBUS guard.
//!
//! The query must write the out-struct - the test seeds it with sentinels
//! and demands `SS_DISABLE` with no stack - because a query that answers 0
//! but writes nothing hands musl uninitialized memory. The install must
//! answer 0. Switching to the alt stack on a real fault needs signal
//! delivery and is deliberately not probed.
//!
//! Beyond the strace's two calls: Linux's refusal set for installs - flag
//! bits the kernel does not know, a stack smaller than MINSIGSTKSZ, and a
//! null base are all refused before anything is recorded - and the
//! read-back of a successful install (same sp and size, clear flags),
//! finished by an SS_DISABLE that a follow-up query confirms.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const SEED: usize = 0x5a5a_5a5a_5a5a_5a5a;

/// The 8 KiB stack Rust std reserves, at a 16-byte-aligned static. Only its
/// address matters; the bytes are never read.
#[repr(align(16))]
#[allow(dead_code)]
struct AltStack([u8; 8192]);
static ALT: AltStack = AltStack([0; 8192]);

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_sigaltstack\n");
    let mut failed = false;

    let mut old = lin::StackT {
        ss_sp: SEED,
        ss_flags: 0x5a5a,
        _pad: 0,
        ss_size: SEED,
    };
    let r = lin::sigaltstack(core::ptr::null(), &mut old);
    lin::check_eq(
        &mut failed,
        b"querying the fresh alt stack succeeds",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the fresh process reports SS_DISABLE with no stack",
        old.ss_flags == lin::SS_DISABLE && old.ss_sp == 0 && old.ss_size == 0,
    );

    // The refusal set: flag bits past SS_DISABLE are checked first, then a
    // stack too small for a signal frame, then a null base - Linux answers
    // ENOMEM, not EINVAL, for the last two.
    let bad_flags = lin::StackT {
        ss_sp: &raw const ALT as *const u8 as usize,
        ss_flags: 0x10,
        _pad: 0,
        ss_size: 8192,
    };
    let r = lin::sigaltstack(&bad_flags, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"an install with unknown ss_flags bits is EINVAL",
        r,
        lin::EINVAL,
    );

    let too_small = lin::StackT {
        ss_sp: &raw const ALT as *const u8 as usize,
        ss_flags: 0,
        _pad: 0,
        ss_size: 1024,
    };
    let r = lin::sigaltstack(&too_small, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"an install below MINSIGSTKSZ is ENOMEM",
        r,
        lin::ENOMEM,
    );

    let null_base = lin::StackT {
        ss_sp: 0,
        ss_flags: 0,
        _pad: 0,
        ss_size: 8192,
    };
    let r = lin::sigaltstack(&null_base, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"an install with a null base is ENOMEM",
        r,
        lin::ENOMEM,
    );

    let ss = lin::StackT {
        ss_sp: &raw const ALT as *const u8 as usize,
        ss_flags: 0,
        _pad: 0,
        ss_size: 8192,
    };
    let r = lin::sigaltstack(&ss, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"installing the 8 KiB alt stack succeeds",
        r,
        0,
    );

    // The install is a fact about the process: a query reads back the same
    // sp and size, with the flag word clear - an enabled stack is flags 0.
    let stack_sp = &raw const ALT as *const u8 as usize;
    let mut old = lin::StackT {
        ss_sp: SEED,
        ss_flags: 0x5a5a,
        _pad: 0,
        ss_size: SEED,
    };
    let r = lin::sigaltstack(core::ptr::null(), &mut old);
    lin::check_eq(
        &mut failed,
        b"querying the installed alt stack succeeds",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the query reports the installed sp and size with clear flags",
        old.ss_sp == stack_sp && old.ss_size == 8192 && old.ss_flags == 0,
    );

    // Disabling is an install of SS_DISABLE, and a query confirms it took.
    let disable = lin::StackT {
        ss_sp: 0,
        ss_flags: lin::SS_DISABLE,
        _pad: 0,
        ss_size: 0,
    };
    let r = lin::sigaltstack(&disable, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"disabling with SS_DISABLE succeeds",
        r,
        0,
    );
    let mut old = lin::StackT {
        ss_sp: SEED,
        ss_flags: 0x5a5a,
        _pad: 0,
        ss_size: SEED,
    };
    let r = lin::sigaltstack(core::ptr::null(), &mut old);
    lin::check_eq(
        &mut failed,
        b"the query after the disable succeeds",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the disabled stack reports SS_DISABLE again",
        old.ss_flags == lin::SS_DISABLE && old.ss_sp == 0 && old.ss_size == 0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_sigaltstack: panicked\n")
}
