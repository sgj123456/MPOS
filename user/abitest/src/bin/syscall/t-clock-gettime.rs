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
//! The null-tp refusal goes through a raw `syscall` stub, since the
//! harness wrapper takes a reference and cannot spell NULL.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::arch::asm;
use core::panic::PanicInfo;

/// `clock_gettime` in its raw two-word shape, for the null-tp refusal the
/// harness wrapper (which takes `&mut TimeSpec`) cannot spell.
fn clock_gettime_raw(clk_id: usize, tp: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") lin::nr::CLOCK_GETTIME as isize => ret,
            in("rdi") clk_id,
            in("rsi") tp,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

/// `clock_nanosleep(clk_id, flags, req, rem)` raw, so a request to sleep on a
/// clock that does not exist can be shown refused before it touches the sleep.
fn clock_nanosleep_raw(clk_id: usize, flags: usize, req: usize, rem: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") lin::nr::CLOCK_NANOSLEEP as isize => ret,
            in("rdi") clk_id,
            in("rsi") flags,
            in("rdx") req,
            in("r10") rem,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-clock-gettime\n");
    let mut failed = false;

    let mut rt = lin::TimeSpec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    let r = lin::clock_gettime(lin::CLOCK_REALTIME, &mut rt);
    lin::check_eq(&mut failed, b"CLOCK_REALTIME answers 0", r, 0);
    lin::check_true(
        &mut failed,
        b"the realtime tv_nsec is inside one second",
        rt.tv_nsec >= 0 && rt.tv_nsec < 1_000_000_000,
    );
    lin::check_true(
        &mut failed,
        b"the realtime tv_sec is non-negative",
        rt.tv_sec >= 0,
    );

    let mut mt = lin::TimeSpec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    let r = lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut mt);
    lin::check_eq(&mut failed, b"CLOCK_MONOTONIC answers 0", r, 0);

    // Two successive reads: the second must be at or after the first.
    let mut rt2 = lin::TimeSpec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    let r = lin::clock_gettime(lin::CLOCK_REALTIME, &mut rt2);
    lin::check_eq(&mut failed, b"a second realtime read answers 0", r, 0);
    lin::check_true(
        &mut failed,
        b"two reads never go backwards",
        rt2.tv_sec > rt.tv_sec || (rt2.tv_sec == rt.tv_sec && rt2.tv_nsec >= rt.tv_nsec),
    );

    // A tp the kernel cannot write to is EFAULT, not a crash.
    let r = clock_gettime_raw(lin::CLOCK_REALTIME, 0);
    lin::check_eq(&mut failed, b"a null tp is EFAULT", r, lin::EFAULT);

    // Sleep 50 ms and demand the monotonic clock noticed: at least 40 ms
    // (the tick is coarse) and at most 5 s (a stuck sleep is a bug).
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
        tv_sec: 0,
        tv_nsec: 0,
    };
    let r = lin::nanosleep(&req, &mut rem);
    lin::check_eq(&mut failed, b"the 50 ms sleep answers 0", r, 0);
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
        b"the sleep moved the monotonic clock by 40 ms to 5 s",
        elapsed_ms >= 40 && elapsed_ms <= 5000,
    );

    // The clock-id vocabulary is closed: Linux accepts exactly {0..=9, 11} and
    // answers EINVAL for the 10 gap and every id from 12 up (measured). This
    // kernel used to fall through to wall-clock time for any id outside its
    // monotonic list, so clock_gettime(999, &ts) answered 0 with a plausible
    // number - a success for a clock never named.
    let mut probe = lin::TimeSpec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    lin::check_eq(
        &mut failed,
        b"clock_gettime(10) - a gap in the id set - is EINVAL",
        lin::clock_gettime(10, &mut probe),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"clock_gettime(999) is EINVAL, not a silent wall reading",
        lin::clock_gettime(999, &mut probe),
        lin::EINVAL,
    );
    // A recognised-but-unserved-static id is still accepted (TAI = 11), so the
    // gate is the vocabulary, not a refusal of everything past the first few.
    lin::check_eq(
        &mut failed,
        b"clock_gettime(11 / CLOCK_TAI) answers 0",
        lin::clock_gettime(11, &mut probe),
        0,
    );

    // clock_nanosleep shares the gate: a nonexistent clock is EINVAL before it
    // sleeps, rather than sleeping on the default and answering 0.
    let one_ns = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 1,
    };
    let mut rem = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    lin::check_eq(
        &mut failed,
        b"clock_nanosleep(999) on a nonexistent clock is EINVAL",
        clock_nanosleep_raw(999, 0, &raw const one_ns as usize, &raw mut rem as usize),
        lin::EINVAL,
    );
    // A real clock still sleeps (relative, flags 0).
    lin::check_eq(
        &mut failed,
        b"clock_nanosleep(CLOCK_MONOTONIC) answers 0",
        clock_nanosleep_raw(
            lin::CLOCK_MONOTONIC,
            0,
            &raw const one_ns as usize,
            &raw mut rem as usize,
        ),
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-clock-gettime: panicked\n")
}
