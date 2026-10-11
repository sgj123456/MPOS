//! t-priority - `setpriority`/`getpriority` round-trip the nice value.
//!
//! The contract pinned here is the round-trip, which the kernel used to fail:
//! `setpriority` accepted any in-range value and dropped it, and `getpriority`
//! answered a constant `20` (the raw encoding of nice 0) whatever had been set.
//! That is the silent-success `NOTES.md` §4 forbids - a caller that set a nice
//! and read it back to decide what to do next was handed a number that had
//! nothing behind it.
//!
//! The raw syscall speaks Linux's `20-nice` encoding (the man page's note on
//! C-library/kernel differences is explicit that the `20-nice` flip is the
//! wrapper's), so nice -5 answers 25 and nice 3 answers 17. Pinned: a fresh
//! process reads back 20 (nice 0); a set is *stored*, not accepted and dropped;
//! a second set round-trips again; an out-of-range nice is **clamped**, not
//! refused; a pid that names no process is `ESRCH`; and a `which` that is not one
//! of PROCESS/PGRP/USER is `EINVAL`.
//!
//! # Why four of the calls are raw
//!
//! The `20-nice` word is the first reason, and it is the rule's clause (c):
//! `rustix::process::getpriority_process` *does* bind the call, but its backend
//! has already converted - `Ok(20 - ret_c_int(...))` - so the ABI word the claim
//! is about cannot be seen through it. A named `libc` wrapper exists too
//! (`libc::getpriority`) and converts the same way, so the word goes through
//! `libc::syscall` with the numbers from `libc::SYS_getpriority`/
//! `SYS_setpriority` - a maintained per-machine table, and one that has the two
//! numbers *swapped* between the machines (140/141 and 141/140), which is worth
//! not writing down by hand.
//!
//! The other reason is the rule's clause (a): `sched_setattr`/`sched_getattr`
//! have no `rustix` binding at all, so the 48-byte `struct sched_attr` goes
//! through `libc::syscall` too. That struct's layout is a UAPI fact
//! (`docs/uapi/` §4 pins it to this program): the size at offset 0, the policy
//! at 4, the nice at 16 - and the nice it carries is the plain `-20..19` value,
//! not the raw `20-nice` the `getpriority` wrapper flips.
//!
//! The two doors do **not** agree about a bad nice, and Linux is that way: the
//! `setpriority` syscall *clamps* (`kernel/sys.c`), which is what the clamp
//! claims below pin, while `sched_setattr` refuses an out-of-range `sched_nice`
//! with `EINVAL` - measured on a current host, where `setpriority(PRIO_PROCESS,
//! 0, 20)` answers `0` and leaves the task at `19`. This kernel answered `EINVAL`
//! from *both* doors, and the no-std version pinned that; both the kernel and the
//! claims moved with migration batch 10.
//!
//! The scheduler still runs on fixed priority bands and a nice does not buy CPU
//! share here (a named sub-gap); what this pins is that the *call* is truthful
//! about the value it stores and reports, which is what programs branch on.

use mpos_user_tests::Report;
use rustix::io::Errno;

const PRIO_PROCESS: i32 = 0;
const PRIO_PGRP: i32 = 1;

/// A pid far past any that exists in either test payload, for the `ESRCH`
/// claim.
const NO_SUCH_PID: i32 = 0x3FFF;

/// A `which` that is none of `PRIO_PROCESS`/`PRIO_PGRP`/`PRIO_USER`.
const NOT_A_WHICH: i32 = 9;

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `getpriority(which, who)`'s **raw** answer: Linux's `20 - nice` word. The
/// module comment says why this does not go through a wrapper.
fn getpriority(which: i32, who: i32) -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_getpriority,
            which as libc::c_long,
            who as libc::c_long,
        )
    }
}

/// `setpriority(which, who, nice)`. The argument is the *nice*, not the raw word:
/// the `20-nice` flip is `getpriority`'s return and nothing else (glibc's
/// wrapper passes `prio` straight through, and so does this kernel's handler).
fn setpriority(which: i32, who: i32, nice: i32) -> Result<(), Errno> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_setpriority,
            which as libc::c_long,
            who as libc::c_long,
            nice as libc::c_long,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(last_errno()))
    }
}

/// The 48-byte `struct sched_attr`'s `sched_nice`, at its own offset.
fn attr_nice(attr: &[u8; 48]) -> i32 {
    i32::from_ne_bytes([attr[16], attr[17], attr[18], attr[19]])
}

fn set_attr_nice(attr: &mut [u8; 48], nice: i32) {
    attr[16..20].copy_from_slice(&nice.to_ne_bytes());
}

/// `sched_getattr(pid 0, attr, size, 0)` - the calling thread's own attributes.
fn sched_getattr(attr: &mut [u8; 48], size: u32) -> Result<(), Errno> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_sched_getattr,
            0 as libc::c_long,
            attr.as_mut_ptr(),
            size as libc::c_long,
            0 as libc::c_long,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(last_errno()))
    }
}

/// `sched_setattr(pid 0, attr, 0)` - the sibling door onto the same nice slot.
fn sched_setattr(attr: &[u8; 48]) -> Result<(), Errno> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_sched_setattr,
            0 as libc::c_long,
            attr.as_ptr(),
            0 as libc::c_long,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(last_errno()))
    }
}

fn main() {
    println!("t-priority");
    let mut r = Report::new();

    // A fresh process is at nice 0, which the raw call reports as 20.
    r.check(
        "a fresh process reads back nice 0 (raw 20)",
        getpriority(PRIO_PROCESS, 0) == 20,
    );

    // Set a real nice and read it back. Before this shipped, the set was
    // dropped and the get answered 20 regardless, so 25 is the pin.
    r.check(
        "setpriority(PRIO_PROCESS, 0, -5) succeeds",
        setpriority(PRIO_PROCESS, 0, -5) == Ok(()),
    );
    r.check(
        "getpriority reads back the nice it was set to (raw 25 = -5)",
        getpriority(PRIO_PROCESS, 0) == 25,
    );

    // Round-trips again to a different value, both directions of the range.
    r.check(
        "setpriority to +3 succeeds",
        setpriority(PRIO_PROCESS, 0, 3) == Ok(()),
    );
    r.check(
        "getpriority reads back 17 (= 20 - 3)",
        getpriority(PRIO_PROCESS, 0) == 17,
    );

    // The whole process group the caller leads is the same set for a
    // single-process group: a group set round-trips through the process get.
    r.check(
        "setpriority(PRIO_PGRP, 0, -2) succeeds",
        setpriority(PRIO_PGRP, 0, -2) == Ok(()),
    );
    r.check(
        "a group set is seen by the process get",
        getpriority(PRIO_PROCESS, 0) == 22,
    );

    // Outside the range the nice is clamped, not refused - see the module
    // comment for the measurement. Each claim makes the call *and* reads it
    // back, because a clamp that answered 0 and stored nothing would pass the
    // first half alone.
    r.check(
        "a nice of 20 is clamped to 19 (raw 1), not refused",
        setpriority(PRIO_PROCESS, 0, 20) == Ok(()) && getpriority(PRIO_PROCESS, 0) == 1,
    );
    r.check(
        "a nice of -21 is clamped to -20 (raw 40)",
        setpriority(PRIO_PROCESS, 0, -21) == Ok(()) && getpriority(PRIO_PROCESS, 0) == 40,
    );

    // A pid that names no process: ESRCH, exactly as Linux answers it.
    r.check(
        "setpriority on a pid that is not a process is ESRCH",
        setpriority(PRIO_PROCESS, NO_SUCH_PID, 0) == Err(Errno::SRCH),
    );
    // A which that is none of the three is EINVAL.
    r.check(
        "an unknown which is EINVAL",
        setpriority(NOT_A_WHICH, 0, 0) == Err(Errno::INVAL),
    );

    // --- sched_setattr / sched_getattr reach the same nice slot ---
    //
    // These are the modern door onto the very value setpriority/getpriority just
    // round-tripped. Before this shipped, sched_setattr returned 0 while dropping
    // the nice and sched_getattr reported a hardcoded 0, so every check below is
    // a pin against that lie: a value written through one door must be readable
    // through the other.
    let mut attr = [0u8; 48];
    attr[0..4].copy_from_slice(&48u32.to_ne_bytes());
    attr[4..8].copy_from_slice(&0u32.to_ne_bytes()); // SCHED_OTHER

    // Set it explicitly, so this does not depend on the value the clamp claims
    // above happened to leave in force.
    r.check(
        "setpriority(-5) again succeeds before the sched door",
        setpriority(PRIO_PROCESS, 0, -5) == Ok(()),
    );
    r.check(
        "setpriority(-5) then sched_getattr succeeds",
        sched_getattr(&mut attr, 48).is_ok(),
    );
    r.check(
        "sched_getattr reads back the nice -5 setpriority stored",
        attr_nice(&attr) == -5,
    );

    // sched_setattr writes a new nice, and getpriority must see it. nice -7 is
    // the raw answer 27 (20 - (-7)).
    set_attr_nice(&mut attr, -7);
    r.check(
        "sched_setattr(nice -7) succeeds",
        sched_setattr(&attr).is_ok(),
    );
    r.check(
        "getpriority reads back the nice sched_setattr stored (raw 27 = -7)",
        getpriority(PRIO_PROCESS, 0) == 27,
    );

    // And the same value reads back through sched_getattr, closing the loop.
    let round_trip = match sched_getattr(&mut attr, 48) {
        Ok(()) => attr_nice(&attr) == -7,
        Err(_) => false,
    };
    r.check(
        "sched_setattr(-7) then sched_getattr reads back -7",
        round_trip,
    );

    // The other door refuses an out-of-range nice rather than clamping it:
    // `sched_setattr` with nice 25 is EINVAL, and `sched_getattr` with a struct
    // smaller than the known version is EINVAL rather than a silent short
    // answer. The asymmetry with `setpriority` above is Linux's.
    set_attr_nice(&mut attr, 25);
    r.check(
        "sched_setattr with a nice of 25 is EINVAL",
        sched_setattr(&attr) == Err(Errno::INVAL),
    );
    r.check(
        "sched_getattr with a 40-byte struct is EINVAL",
        sched_getattr(&mut attr, 40) == Err(Errno::INVAL),
    );

    // Leaving the caller back at nice 0 does not affect the rest of the suite.
    let _ = setpriority(PRIO_PROCESS, 0, 0);
    r.finish()
}
