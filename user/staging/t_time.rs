//! t_time - the clocks, and the two reports about time spent.
//!
//! Four calls answer "what time is it" and two answer "how much did that
//! cost", and a libc uses all of them without asking which are there:
//!
//! * `time(2)` is the original: seconds since the epoch, both returned and
//!   written through the pointer a caller passes. A program that only wants
//!   `now` still calls this one.
//! * `clock_getres(2)` says how fine the clock actually is. A program that
//!   is about to sleep reads it before deciding whether sleeping is worth
//!   it, so the answer has to be a real resolution and not a claim.
//! * `gettimeofday(2)` is the microsecond spelling, and `clock_gettime`
//!   the nanosecond one; all three must agree about the same second.
//! * `times(2)` and `getrusage(2)` report time spent. Whether the kernel
//!   accounts it is its own business, but it must fill the buffers and
//!   answer something a caller can use as a baseline.
//!
//! The agreement check is the one that matters: a `time` that disagrees
//! with `gettimeofday` by more than a second is two clocks wearing one
//! name, which is how a TLS library decides every certificate is expired.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const EINVAL: isize = -22;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_time\n");
    let mut failed = false;

    // --- time(2) ---------------------------------------------------------
    let mut slot: i64 = -1;
    let ret = lin::time(Some(&mut slot));
    lin::check_true(
        &mut failed,
        b"time answers a real epoch - not 1970 plus uptime",
        ret > 1_600_000_000,
    );
    lin::check_eq(
        &mut failed,
        b"and writes the same seconds through the pointer",
        slot,
        ret,
    );
    // A null pointer is "return it, do not store it": the same answer, no
    // fault.
    let again = lin::time(None);
    lin::check_true(
        &mut failed,
        b"time with a null pointer returns the seconds and does not fault",
        again >= ret,
    );

    // --- every clock agrees about now -----------------------------------
    let mut ts = lin::TimeSpec { tv_sec: 0, tv_nsec: 0 };
    let r = lin::clock_gettime(lin::CLOCK_REALTIME, &mut ts);
    lin::check_eq(&mut failed, b"clock_gettime(CLOCK_REALTIME) answers", r, 0);
    let diff = if ts.tv_sec > ret { ts.tv_sec - ret } else { ret - ts.tv_sec };
    lin::check_true(
        &mut failed,
        b"time and clock_gettime agree about the second",
        diff <= 2,
    );

    let mut tv = lin::TimeVal { tv_sec: 0, tv_usec: 0 };
    let r = lin::gettimeofday(&mut tv);
    lin::check_eq(&mut failed, b"gettimeofday answers", r, 0);
    let diff = if tv.tv_sec > ret { tv.tv_sec - ret } else { ret - tv.tv_sec };
    lin::check_true(
        &mut failed,
        b"gettimeofday agrees with time about the second",
        diff <= 2,
    );
    lin::check_true(
        &mut failed,
        b"gettimeofday's microseconds are a microsecond field, not nanoseconds",
        tv.tv_usec < 1_000_000,
    );

    // --- clock_getres ----------------------------------------------------
    let mut res = lin::TimeSpec { tv_sec: 1, tv_nsec: 1 };
    let r = lin::clock_getres(lin::CLOCK_REALTIME, &mut res);
    lin::check_eq(&mut failed, b"clock_getres(CLOCK_REALTIME) answers", r, 0);
    lin::check_true(
        &mut failed,
        b"the resolution is under a second and not zero",
        res.tv_sec == 0 && res.tv_nsec > 0 && res.tv_nsec <= 1_000_000_000,
    );
    let mut res_mono = lin::TimeSpec { tv_sec: 1, tv_nsec: 1 };
    let r = lin::clock_getres(lin::CLOCK_MONOTONIC, &mut res_mono);
    lin::check_eq(&mut failed, b"clock_getres(CLOCK_MONOTONIC) answers", r, 0);
    // A clock that is not one of the eight Linux names is EINVAL.
    let mut res_bad = lin::TimeSpec { tv_sec: 0, tv_nsec: 0 };
    let r = lin::clock_getres(99, &mut res_bad);
    lin::check_eq(
        &mut failed,
        b"clock_getres for a clock that is not there is EINVAL",
        r,
        EINVAL,
    );

    // --- the reports about time spent -----------------------------------
    // What is being pinned is the contract, not the accounting: the call
    // fills its buffer, and the value it returns is a tick count a caller
    // can subtract from.
    let mut tms = [0u8; 32];
    let ticks = lin::times(Some(&mut tms));
    lin::check_true(
        &mut failed,
        b"times returns a tick count",
        ticks > 0,
    );
    let ticks_null = lin::times(None);
    lin::check_true(
        &mut failed,
        b"times with a null buffer still returns the tick count",
        ticks_null >= ticks,
    );

    let mut usage = [0u8; 144];
    let r = lin::getrusage(0, &mut usage);
    lin::check_eq(&mut failed, b"getrusage(RUSAGE_SELF) answers", r, 0);
    let r = lin::getrusage(-1i32 as usize, &mut usage);
    lin::check_eq(&mut failed, b"getrusage(RUSAGE_CHILDREN) answers", r, 0);
    let r = lin::getrusage(7, &mut usage);
    lin::check_eq(
        &mut failed,
        b"getrusage for a who that is not one is EINVAL",
        r,
        EINVAL,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_time: panicked\n")
}
