//! t-rseq - one test for rseq, which glibc registers at startup
//! (`rseq({...}, 33, 0, 0x53053053) = 0` in the strace): the
//! restartable-sequence block every future glibc binary will present.
//!
//! Pinned here: registering the 32-byte struct rseq with glibc's signature
//! 0x53053053 succeeds. Whether the kernel then maintains cpu_id fields on
//! preemption is out of scope - registration is what startup needs to get
//! past.
//!
//! The refusals Linux pins are pinned too: a length other than 32 and a
//! flag word other than 0 are EINVAL. A signature of 0 is accepted (the
//! field is recorded, never acted on), and re-registering with the same
//! arguments succeeds again - nothing is kept that could see a second
//! registration as a conflict.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut RSEQ: core::mem::MaybeUninit<lin::Rseq> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-rseq\n");
    let mut failed = false;

    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 32, 0, lin::RSEQ_SIG);
    lin::check_eq(
        &mut failed,
        b"registering the rseq block with glibc's signature succeeds",
        r,
        0,
    );

    // Linux pins the length to sizeof(struct rseq) = 32; 31 is one short.
    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 31, 0, lin::RSEQ_SIG);
    lin::check_eq(&mut failed, b"a length of 31 is EINVAL", r, lin::EINVAL);

    // The flag word must be 0: there is no flag the kernel knows.
    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 32, 1, lin::RSEQ_SIG);
    lin::check_eq(&mut failed, b"a flag word of 1 is EINVAL", r, lin::EINVAL);

    // The signature is recorded and never acted on, so 0 is as good as
    // glibc's.
    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 32, 0, 0);
    lin::check_eq(&mut failed, b"a signature of 0 is accepted", r, 0);

    // Re-registration with the same arguments succeeds: the kernel keeps no
    // registration state that could see the second call as a conflict.
    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 32, 0, lin::RSEQ_SIG);
    lin::check_eq(
        &mut failed,
        b"re-registering with the same arguments succeeds",
        r,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-rseq: panicked\n")
}
