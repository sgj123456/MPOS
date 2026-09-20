//! t-sched-getaffinity - one test for sched_getaffinity, which the strace
//! shows at startup (`sched_getaffinity(pid, 32, mask) = 8` on the 16-core
//! host): Rust std's available_parallelism asks through it.
//!
//! Pinned here: the return value is **not** the CPU count. Linux answers the
//! number of bytes it placed in the mask - the strace line above says `= 8`,
//! the one word of this machine's mask - and a caller that wants a count asks
//! the bits, which is what Rust std's available_parallelism does. mpos
//! schedules on one CPU, so the mask is bit 0 with bits 1-63 clear: one word,
//! one CPU, no stray bits. The mask size argument travels as the syscall's
//! second word (8 bytes here, matching the one u64 the test hands over). A
//! pid of -1 - "some other process, please" in the Linux shape - is answered
//! the same way: mpos does not track per-process affinities.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// What every successful call answers: the one word Linux copied in, which
/// is how many bytes - not how many CPUs - the mask carries.
const MASK_BYTES: isize = 8;

static mut MASK: u64 = 0;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-sched-getaffinity\n");
    let mut failed = false;

    let mask = &raw mut MASK;
    let n = lin::sched_getaffinity(0, mask);
    lin::check_eq(
        &mut failed,
        b"the affinity answer is the byte count of the mask filled",
        n,
        MASK_BYTES,
    );
    lin::check_true(
        &mut failed,
        b"that CPU is CPU 0",
        unsafe { (*mask) & 1 == 1 },
    );
    lin::check_true(
        &mut failed,
        b"bits 1-63 of the mask are clear",
        unsafe { (*mask) & !1 == 0 },
    );

    // The pid argument does not steer anything: -1 answers just like 0,
    // with the same one-CPU mask.
    let n = lin::sched_getaffinity(-1, mask);
    lin::check_eq(
        &mut failed,
        b"a pid of -1 answers the same mask size",
        n,
        MASK_BYTES,
    );
    lin::check_true(
        &mut failed,
        b"the -1 call fills bit 0 too",
        unsafe { (*mask) & 1 == 1 },
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-sched-getaffinity: panicked\n")
}
