//! t-rseq - one test for rseq, the restartable-sequence block a glibc
//! registers at startup. Pinned here is the honest *refusal*: this kernel
//! carries no per-thread `rseq` registration, so it cannot maintain the
//! `cpu_id`/`node_id`/`mm_cid` fields the ABI contract puts in the caller's
//! block, nor honour `RSEQ_FLAG_UNREGISTER` - and the syscall therefore answers
//! `ENOSYS` ("not available"), which is exactly the fallback a libc that probes
//! `rseq` and finds none is written to take.
//!
//! The alternative - what used to live here - was returning `0` for a
//! well-formed 32-byte registration while writing none of those fields: a
//! success for a thing not done, which `AGENTS.md` §4.2 forbids. Two
//! measurements confirm `ENOSYS` is the truthful answer rather than a regression:
//! the libc this machine actually runs (musl) never calls `rseq`, and on a
//! current host (Linux 7.2, `AT_RSEQ_FEATURE_SIZE` 33, `AT_RSEQ_ALIGN` 64) glibc
//! registers at length *33* - a shape the old length-equals-32 arm rejected with
//! `EINVAL` anyway, so it honoured neither real glibc nor the contract.
//!
//! The refusal is on *capability*, not argument shape, so every spelling answers
//! `ENOSYS`: the call is never examined, because there is nothing here that
//! could act on any of its arguments.

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

    // A well-formed registration: 32 bytes, flag word 0, glibc's signature.
    // The kernel has no registration to file it against, so it refuses rather
    // than pretend to accept it - the counterfactual of the old hollow `0`.
    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 32, 0, lin::RSEQ_SIG);
    lin::check_eq(
        &mut failed,
        b"a well-formed rseq registration answers ENOSYS, not a false 0",
        r,
        lin::ENOSYS,
    );

    // The modern glibc spelling (length 33) and an unregister request answer the
    // same ENOSYS: the refusal is on capability, not on how the call is shaped.
    let r = lin::rseq(&raw mut RSEQ as *mut lin::Rseq, 33, 0, lin::RSEQ_SIG);
    lin::check_eq(
        &mut failed,
        b"the length-33 spelling answers ENOSYS too",
        r,
        lin::ENOSYS,
    );
    let r = lin::rseq(
        &raw mut RSEQ as *mut lin::Rseq,
        32,
        1, /* UNREGISTER */
        lin::RSEQ_SIG,
    );
    lin::check_eq(
        &mut failed,
        b"an unregister request answers ENOSYS too",
        r,
        lin::ENOSYS,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-rseq: panicked\n")
}
