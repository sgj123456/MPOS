//! t_sigaltstack - one test for sigaltstack, which the musl_hello strace
//! uses twice: a query of the fresh process (`sigaltstack(NULL,
//! {SS_DISABLE})`) and the install of the 8 KiB stack Rust std reserved
//! with mmap for its SIGSEGV/SIGBUS guard.
//!
//! The query must write the out-struct - the test seeds it with sentinels
//! and demands `SS_DISABLE` with no stack - because a query that answers 0
//! but writes nothing hands musl uninitialized memory. The install must
//! answer 0. Switching to the alt stack on a real fault needs signal
//! delivery and is deliberately not probed.

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

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_sigaltstack: panicked\n")
}
