//! t-nanosleep - one test for nanosleep and its clock-aware sibling
//! clock_nanosleep.
//!
//! The man page pins: nanosleep(req, rem) sleeps out the requested
//! `struct timespec` - with tv_nsec strictly below a second, since a
//! billion nanoseconds is tv_sec's job - and answers 0, writing the
//! remaining time into rem only if something interrupted the sleep. Pinned
//! here: a 50 ms request answers 0 and really takes somewhere between 40 ms
//! and 5 s (the tick is coarse below; a stuck sleep is a bug above), a
//! zero request answers 0 immediately, and tv_nsec of exactly 1e9 is
//! EINVAL. clock_nanosleep sleeps relatively whatever clock it names, and
//! refuses TIMER_ABSTIME with EINVAL - mpos keeps no absolute time to be
//! measured against, a documented refusal rather than a wrong duration.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-nanosleep\n");
    let mut failed = false;

    // The 50 ms sleep, measured against the monotonic clock: the call must
    // both answer 0 and take a sane amount of wall time.
    let mut before = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let r = lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut before);
    lin::check_eq(&mut failed, b"the pre-sleep monotonic read answers 0", r, 0);
    let req = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    let mut rem = lin::TimeSpec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    let r = lin::nanosleep(&req, &mut rem);
    lin::check_eq(&mut failed, b"nanosleep of 50 ms answers 0", r, 0);
    let mut after = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let r = lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut after);
    lin::check_eq(
        &mut failed,
        b"the post-sleep monotonic read answers 0",
        r,
        0,
    );
    let elapsed_ms =
        (after.tv_sec - before.tv_sec) * 1000 + (after.tv_nsec - before.tv_nsec) / 1_000_000;
    lin::check_true(
        &mut failed,
        b"the 50 ms sleep took 40 ms to 5 s of wall time",
        elapsed_ms >= 40 && elapsed_ms <= 5000,
    );

    // A zero request is over before it starts - and must not hang.
    let zero = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let r = lin::nanosleep(&zero, &mut rem);
    lin::check_eq(
        &mut failed,
        b"nanosleep of zero answers 0 immediately",
        r,
        0,
    );

    // A billion nanoseconds belong in tv_sec: tv_nsec at exactly 1e9 is
    // the man page's EINVAL.
    let bad = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 1_000_000_000,
    };
    let r = lin::nanosleep(&bad, &mut rem);
    lin::check_eq(
        &mut failed,
        b"tv_nsec of exactly one second is EINVAL",
        r,
        lin::EINVAL,
    );

    // Relative clock_nanosleep: whatever clock it names, the relative
    // duration is what gets slept - 1 ms here, answered 0.
    let ms1 = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    let r = lin::clock_nanosleep(lin::CLOCK_MONOTONIC, 0, &ms1, &mut rem);
    lin::check_eq(
        &mut failed,
        b"a 1 ms relative clock_nanosleep answers 0",
        r,
        0,
    );

    // TIMER_ABSTIME needs an absolute time to be measured against, and mpos
    // keeps none: the refusal is EINVAL, by design, rather than a sleep of
    // the wrong duration.
    let r = lin::clock_nanosleep(lin::CLOCK_REALTIME, 1, &ms1, &mut rem);
    lin::check_eq(
        &mut failed,
        b"TIMER_ABSTIME is refused with EINVAL",
        r,
        lin::EINVAL,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-nanosleep: panicked\n")
}
