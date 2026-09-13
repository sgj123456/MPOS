//! t_sched_getaffinity - one test for sched_getaffinity, which the strace
//! shows at startup (`sched_getaffinity(pid, 32, mask) = 8` on the 16-core
//! host): Rust std's available_parallelism asks through it.
//!
//! Pinned here: mpos schedules on one CPU, so the call must answer 1 - the
//! count of CPUs in the mask - and fill bit 0, with bits 1-63 clear: one
//! word, one CPU, no stray bits. The mask size argument travels as the
//! syscall's second word (8 bytes here, matching the one u64 the test hands
//! over). A pid of -1 - "some other process, please" in the Linux shape -
//! is answered the same way: mpos does not track per-process affinities.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut MASK: u64 = 0;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_sched_getaffinity\n");
    let mut failed = false;

    let mask = &raw mut MASK;
    let n = lin::sched_getaffinity(0, mask);
    lin::check_eq(
        &mut failed,
        b"the affinity mask covers exactly one CPU",
        n,
        1,
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
        b"a pid of -1 answers the same one CPU",
        n,
        1,
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
    lin::die(b"t_sched_getaffinity: panicked\n")
}
