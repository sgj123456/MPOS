//! t_rseq - one test for rseq, which glibc registers at startup
//! (`rseq({...}, 33, 0, 0x53053053) = 0` in the strace): the
//! restartable-sequence block every future glibc binary will present.
//!
//! Pinned here: registering the 32-byte struct rseq with glibc's signature
//! 0x53053053 succeeds. Whether the kernel then maintains cpu_id fields on
//! preemption is out of scope - registration is what startup needs to get
//! past.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut RSEQ: core::mem::MaybeUninit<lin::Rseq> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_rseq\n");
    let mut failed = false;

    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 32, 0, lin::RSEQ_SIG);
    lin::check_eq(
        &mut failed,
        b"registering the rseq block with glibc's signature succeeds",
        r,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_rseq: panicked\n")
}
