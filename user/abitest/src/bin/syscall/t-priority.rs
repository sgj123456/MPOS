//! t-priority - `setpriority`/`getpriority` round-trip the nice value.
//!
//! The contract pinned here is the round-trip, which the kernel used to fail:
//! `setpriority` accepted any in-range value and dropped it, and `getpriority`
//! answered a constant `20` (the raw encoding of nice 0) whatever had been set.
//! That is the silent-success `PLAN.md` §4 forbids - a caller that set a nice
//! and read it back to decide what to do next was handed a number that had
//! nothing behind it.
//!
//! The raw syscall speaks Linux's `20-nice` encoding (the man page's note on
//! C-library/kernel differences is explicit that the `20-nice` flip is the
//! wrapper's), so nice -5 answers 25 and nice 3 answers 17. Pinned:
//!
//! * a fresh process reads back 20 (nice 0);
//! * `setpriority(PRIO_PROCESS, 0, -5)` succeeds and `getpriority` then answers
//!   25 - the value is stored, not accepted and dropped;
//! * a second set to a legal range and back round-trips again;
//! * an out-of-range nice (`-21`, `20`) is `EINVAL`;
//! * `setpriority` on a pid that names no process is `ESRCH`, as Linux's;
//! * a `which` that is not one of PROCESS/PGRP/USER is `EINVAL`.
//!
//! The scheduler still runs on fixed priority bands and a nice does not buy
//! CPU share here (a named sub-gap); what this pins is that the *call* is
//! truthful about the value it stores and reports, which is what programs
//! branch on.
//!
//! The modern door `sched_setattr`/`sched_getattr` reaches the same per-task
//! nice slot, and used to answer the same lie: `sched_setattr` returned `0`
//! while dropping the nice, and `sched_getattr` reported a hardcoded `0`
//! whatever the task was actually running at. That round-trip is pinned here
//! too - a value written through one door is read back through the other,
//! and both refuse an out-of-range nice.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const PRIO_PROCESS: usize = 0;
const PRIO_PGRP: usize = 1;
const PRIO_USER: usize = 2;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-priority\n");
    let mut failed = false;

    // A fresh process is at nice 0, which the raw call reports as 20.
    lin::check_eq(
        &mut failed,
        b"a fresh process reads back nice 0 (raw 20)",
        lin::getpriority(PRIO_PROCESS, 0),
        20,
    );

    // Set a real nice and read it back. Before this shipped, the set was
    // dropped and the get answered 20 regardless, so 25 is the pin.
    lin::check_eq(
        &mut failed,
        b"setpriority(PRIO_PROCESS, 0, -5) succeeds",
        lin::setpriority(PRIO_PROCESS, 0, -5),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getpriority reads back the nice it was set to (raw 25 = -5)",
        lin::getpriority(PRIO_PROCESS, 0),
        25,
    );

    // Round-trips again to a different value, both directions of the range.
    lin::check_eq(
        &mut failed,
        b"setpriority to +3 succeeds",
        lin::setpriority(PRIO_PROCESS, 0, 3),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getpriority reads back 17 (= 20 - 3)",
        lin::getpriority(PRIO_PROCESS, 0),
        17,
    );

    // The whole process group the caller leads is the same set for a
    // single-process group: a group set round-trips through the process get.
    lin::check_eq(
        &mut failed,
        b"setpriority(PRIO_PGRP, 0, -2) succeeds",
        lin::setpriority(PRIO_PGRP, 0, -2),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a group set is seen by the process get",
        lin::getpriority(PRIO_PROCESS, 0),
        22,
    );

    // Out-of-range nice is EINVAL, both below -20 and above 19.
    lin::check_eq(
        &mut failed,
        b"a nice of -21 is EINVAL",
        lin::setpriority(PRIO_PROCESS, 0, -21),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a nice of 20 is EINVAL",
        lin::setpriority(PRIO_PROCESS, 0, 20),
        lin::EINVAL,
    );

    // A pid that names no process: ESRCH, exactly as Linux answers it. Use a
    // pid far past any that exist in the test initramfs.
    lin::check_eq(
        &mut failed,
        b"setpriority on a pid that is not a process is ESRCH",
        lin::setpriority(PRIO_PROCESS, 0x3FFF, 0),
        lin::ESRCH,
    );

    // A which that is none of the three is EINVAL.
    lin::check_eq(
        &mut failed,
        b"an unknown which is EINVAL",
        lin::setpriority(9, 0, 0),
        lin::EINVAL,
    );

    // --- sched_setattr / sched_getattr reach the same nice slot ---
    //
    // These are the modern door onto the very value setpriority/getpriority
    // just round-tripped. Before this shipped, sched_setattr returned 0 while
    // dropping the nice and sched_getattr reported a hardcoded 0, so every
    // check below is a pin against that lie: a value written through one door
    // must be readable through the other.
    //
    // The 48-byte struct lays the size at offset 0, the policy at 4, the nice
    // at 16. The nice the struct carries is the plain -20..19 value, not the
    // raw 20-nice the getpriority wrapper flips.
    let mut attr = [0u8; 48];
    attr[0..4].copy_from_slice(&48u32.to_ne_bytes());
    attr[4..8].copy_from_slice(&0u32.to_ne_bytes()); // SCHED_OTHER

    // setpriority leaves the task at nice -5; sched_getattr must read -5, not 0.
    // Set it explicitly here so this check does not depend on the value the
    // group-set step above happened to leave in force.
    lin::check_eq(
        &mut failed,
        b"setpriority(-5) again succeeds before the sched door",
        lin::setpriority(PRIO_PROCESS, 0, -5),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"setpriority(-5) then sched_getattr succeeds",
        lin::sched_getattr(attr.as_mut_ptr(), 48, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"sched_getattr reads back the nice -5 setpriority stored",
        i32::from_ne_bytes([attr[16], attr[17], attr[18], attr[19]]) as isize,
        -5,
    );

    // sched_setattr writes a new nice, and getpriority must see it. nice -7
    // is the raw answer 27 (20 - (-7)).
    attr[16..20].copy_from_slice(&(-7i32).to_ne_bytes());
    lin::check_eq(
        &mut failed,
        b"sched_setattr(nice -7) succeeds",
        lin::sched_setattr(attr.as_ptr(), 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"getpriority reads back the nice sched_setattr stored (raw 27 = -7)",
        lin::getpriority(PRIO_PROCESS, 0),
        27,
    );

    // And the same value reads back through sched_getattr, closing the loop.
    lin::check_eq(
        &mut failed,
        b"sched_setattr(-7) then sched_getattr reads back -7",
        {
            let rc = lin::sched_getattr(attr.as_mut_ptr(), 48, 0);
            if rc == 0 {
                i32::from_ne_bytes([attr[16], attr[17], attr[18], attr[19]]) as isize
            } else {
                rc
            }
        },
        -7,
    );

    // Both doors refuse an out-of-range nice: sched_setattr with nice 25 is
    // EINVAL, and sched_getattr with a struct smaller than the known version
    // is EINVAL rather than a silent short answer.
    attr[16..20].copy_from_slice(&(25i32).to_ne_bytes());
    lin::check_eq(
        &mut failed,
        b"sched_setattr with a nice of 25 is EINVAL",
        lin::sched_setattr(attr.as_ptr(), 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"sched_getattr with a 40-byte struct is EINVAL",
        lin::sched_getattr(attr.as_mut_ptr(), 40, 0),
        lin::EINVAL,
    );

    // Leaving the caller back at nice 0 does not affect the rest of the suite.
    let _ = lin::setpriority(PRIO_PROCESS, 0, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-priority: panicked\n")
}
