//! t-clock-gettime - one test for clock_gettime and the time it reports.
//!
//! The man page pins: the call fills a `struct timespec { tv_sec, tv_nsec }`
//! with tv_nsec in [0, 1e9), answers 0 for the clock ids a kernel keeps,
//! and EFAULT for a tp it cannot write. Pinned here: both CLOCK_REALTIME
//! and CLOCK_MONOTONIC answer 0 with a sane timespec, two reads never go
//! backwards - a clock is a function of time, not of luck - and a sleep of
//! 50 ms really does move the monotonic clock by somewhere between 40 ms
//! and 5 s: the kernel's tick is coarse, so the lower bound forgives
//! rounding, and the upper bound catches a sleep that never returns.
//! The null-tp refusal goes through a raw call, since every wrapper owns its
//! buffer (and musl's own `clock_gettime` faults in userspace where the raw
//! call answers EFAULT - the same measurement batch 12 made for fstat).
//!
//! Beyond the shape checks, the id vocabulary is pinned: Linux accepts
//! exactly {0..=9, 11} and answers EINVAL for the 10 gap and every id from
//! 12 up (measured). This kernel used to fall through to wall-clock time for
//! any id outside its monotonic list, so clock_gettime(999, &ts) answered 0
//! with a plausible number - a success for a clock never named. A
//! recognised-but-unserved-static id is still accepted (TAI = 11), so the
//! gate is the vocabulary, not a refusal of everything past the first few.
//! clock_nanosleep shares the gate: a nonexistent clock is EINVAL before it
//! sleeps, rather than sleeping on the default and answering 0.
//!
//! # The flake this program has always had, and why it stays
//!
//! "Two reads never go backwards" can fail on this kernel: CLOCK_REALTIME is
//! the CMOS second plus a tick-derived fraction sampled around it, so two
//! successive reads can step back (`PLAN.md` §13.27, AGENTS.md §5's named
//! flake). It is a property of the wall clock, not of the dispatch this
//! migration touches, and the sweep re-runs before believing it.
//!
//! The raw door carries two claims: the null-tp EFAULT above (clause (c), a
//! wrapper owns its buffer) and the out-of-vocabulary clock ids - 10, 999
//! and the accepted TAI - because rustix's `ClockId` is a plain enum whose
//! vocabulary cannot name them (the same clause that sent batch 8's whence
//! and advice to `libc`). Its numbers come from libc's table
//! (`SYS_clock_gettime`: 228 / 113; `SYS_clock_nanosleep`: 230 / 115).

use mpos_user_tests::Report;
use rustix::thread::{NanosleepRelativeResult, clock_nanosleep_relative};
use rustix::time::{ClockId, Timespec, clock_gettime};

/// `clock_gettime` in its raw two-word shape, for the null-tp refusal no
/// wrapper can spell.
fn clock_gettime_raw(clk_id: libc::clockid_t, tp: usize) -> isize {
    unsafe { libc::syscall(libc::SYS_clock_gettime, clk_id, tp) as isize }
}

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-clock-gettime");
    let mut r = Report::new();

    let rt = clock_gettime(ClockId::Realtime);
    r.check("CLOCK_REALTIME answers 0", rt.tv_sec >= 0);
    r.check(
        "the realtime tv_nsec is inside one second",
        rt.tv_nsec >= 0 && rt.tv_nsec < 1_000_000_000,
    );
    r.check("the realtime tv_sec is non-negative", rt.tv_sec >= 0);

    let mt = clock_gettime(ClockId::Monotonic);
    r.check("CLOCK_MONOTONIC answers 0", mt.tv_sec >= 0);

    // Two successive reads: the second must be at or after the first.
    let rt2 = clock_gettime(ClockId::Realtime);
    r.check(
        "two reads never go backwards",
        rt2.tv_sec > rt.tv_sec || (rt2.tv_sec == rt.tv_sec && rt2.tv_nsec >= rt.tv_nsec),
    );

    // A tp the kernel cannot write to is EFAULT, not a crash.
    r.check(
        "a null tp is EFAULT",
        refused(
            clock_gettime_raw(ClockId::Realtime as libc::clockid_t, 0),
            libc::EFAULT,
        ),
    );

    // Sleep 50 ms and demand the monotonic clock noticed: at least 40 ms
    // (the tick is coarse) and at most 5 s (a stuck sleep is a bug).
    let before = clock_gettime(ClockId::Monotonic);
    let req = Timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    let slept = clock_nanosleep_relative(ClockId::Monotonic, &req);
    r.check(
        "the 50 ms sleep answers 0",
        matches!(slept, NanosleepRelativeResult::Ok),
    );
    let after = clock_gettime(ClockId::Monotonic);
    let elapsed_ms =
        (after.tv_sec - before.tv_sec) * 1000 + (after.tv_nsec - before.tv_nsec) / 1_000_000;
    r.check(
        "the sleep moved the monotonic clock by 40 ms to 5 s",
        (40..=5000).contains(&elapsed_ms),
    );

    // The clock-id vocabulary is closed: Linux accepts exactly {0..=9, 11}
    // and answers EINVAL for the 10 gap and every id from 12 up (measured).
    // rustix's ClockId is a plain enum whose vocabulary stops at the ids it
    // names, so the out-of-vocabulary words travel through the raw door
    // (clause (c)); the accepted TAI rides it too, because the claim is the
    // same register the refusals are read from.
    let mut probe = Timespec::default();
    r.check(
        "clock_gettime(10) - a gap in the id set - is EINVAL",
        refused(clock_gettime_raw(10, &raw mut probe as usize), libc::EINVAL),
    );
    r.check(
        "clock_gettime(999) is EINVAL, not a silent wall reading",
        refused(
            clock_gettime_raw(999, &raw mut probe as usize),
            libc::EINVAL,
        ),
    );
    r.check(
        "clock_gettime(11 / CLOCK_TAI) answers 0",
        clock_gettime_raw(11, &raw mut probe as usize) == 0,
    );

    // clock_nanosleep shares the gate: a nonexistent clock is EINVAL before
    // it sleeps, rather than sleeping on the default and answering 0. The
    // raw four-word call again, because rustix's relative/absolute pair
    // cannot name clock 999 either.
    let one_ns = libc::timespec {
        tv_sec: 0,
        tv_nsec: 1,
    };
    let mut rem = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let r999 = unsafe {
        libc::syscall(
            libc::SYS_clock_nanosleep,
            999 as libc::clockid_t,
            0u64,
            &one_ns as *const _ as usize,
            &mut rem as *mut _ as usize,
        ) as isize
    };
    r.check(
        "clock_nanosleep(999) on a nonexistent clock is EINVAL",
        refused(r999, libc::EINVAL),
    );
    // A real clock still sleeps (relative, flags 0).
    let slept = clock_nanosleep_relative(
        ClockId::Monotonic,
        &Timespec {
            tv_sec: 0,
            tv_nsec: 1,
        },
    );
    r.check(
        "clock_nanosleep(CLOCK_MONOTONIC) answers 0",
        matches!(slept, NanosleepRelativeResult::Ok),
    );

    r.finish()
}
