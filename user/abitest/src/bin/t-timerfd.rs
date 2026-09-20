//! t-timerfd - the timer that reports through a descriptor.
//!
//! Pinned here, per timerfd_create(2): the five clocks are the ones the
//! call takes (and a CPU-time clock is EINVAL), the create flags are
//! TFD_NONBLOCK and TFD_CLOEXEC alone, a read shorter than eight bytes is
//! the man page's own EINVAL, an empty nonblocking read is EAGAIN, a
//! disarmed timer gettimes as zeros, an arm is visible in the remaining
//! time, a one-shot's read hands back exactly one expiration, an interval
//! timer's read hands back the pile that built up, and the disarm a zero
//! `it_value` spells is real.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// Sleep out a whole number of milliseconds, via the harness's nanosleep.
fn sleep_ms(ms: i64) {
    let req = lin::TimeSpec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    let mut rem = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let _ = lin::nanosleep(&req, &mut rem);
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-timerfd\n");
    let mut failed = false;

    // The clockids the call takes - and the CPU-time one it does not.
    lin::check_eq(
        &mut failed,
        b"a CPU-time clock answers EINVAL",
        lin::timerfd_create(lin::CLOCK_PROCESS_CPUTIME_ID, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"unknown create flags answer EINVAL",
        lin::timerfd_create(lin::CLOCK_MONOTONIC as i32, 0xFFFF),
        lin::EINVAL,
    );
    let fd = lin::timerfd_create(lin::CLOCK_MONOTONIC as i32, 0) as i32;
    lin::check_true(&mut failed, b"create hands back a descriptor", fd >= 0);

    // The read's own validation: fewer than eight bytes is EINVAL, and an
    // empty nonblocking timer is EAGAIN rather than a park.
    static mut FOUR: [u8; 4] = [0; 4];
    let r = lin::read(fd, unsafe {
        core::slice::from_raw_parts_mut(&raw mut FOUR as *mut u8, 4)
    });
    lin::check_eq(&mut failed, b"a 4-byte read answers EINVAL", r, lin::EINVAL);

    let nb = lin::timerfd_create(lin::CLOCK_MONOTONIC as i32, lin::TFD_NONBLOCK) as i32;
    lin::check_true(&mut failed, b"the nonblocking create succeeds", nb >= 0);
    static mut EIGHT: [u8; 8] = [0; 8];
    let r = lin::read(nb, unsafe {
        core::slice::from_raw_parts_mut(&raw mut EIGHT as *mut u8, 8)
    });
    lin::check_eq(
        &mut failed,
        b"an empty nonblocking read answers EAGAIN",
        r,
        lin::EAGAIN,
    );

    // A disarmed timer gettimes as zeros throughout.
    let mut curr = lin::Itimerspec::new();
    lin::check_eq(
        &mut failed,
        b"gettime on a fresh timer succeeds",
        lin::timerfd_gettime(fd, &mut curr),
        0,
    );
    lin::check_true(
        &mut failed,
        b"a disarmed timer reports zeros",
        curr.value.tv_sec == 0 && curr.value.tv_nsec == 0 && curr.interval.tv_sec == 0,
    );

    // Arm 200ms, once. The remaining time is somewhere inside the span,
    // and the read after it passes hands back exactly one expiration.
    let arm = lin::Itimerspec {
        interval: lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        value: lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        },
    };
    let mut old = lin::Itimerspec::new();
    lin::check_eq(
        &mut failed,
        b"settime arming 200ms succeeds",
        lin::timerfd_settime(fd, 0, &arm, &raw mut old as *mut lin::Itimerspec),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the old setting of a fresh disarm is zeros",
        old.value.tv_sec == 0 && old.value.tv_nsec == 0,
    );
    lin::check_eq(
        &mut failed,
        b"gettime on the armed timer succeeds",
        lin::timerfd_gettime(fd, &mut curr),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the remaining time is inside the armed span",
        curr.value.tv_sec == 0 && curr.value.tv_nsec > 0 && curr.value.tv_nsec <= 200_000_000,
    );

    // A gettime on something that is not a timer: EINVAL.
    let mut pipe_fds: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the pipe pair is made",
        lin::pipe2(&mut pipe_fds, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"gettime on a pipe answers EINVAL",
        lin::timerfd_gettime(pipe_fds[0], &mut curr),
        lin::EINVAL,
    );

    sleep_ms(300);
    let r = lin::read(fd, unsafe {
        core::slice::from_raw_parts_mut(&raw mut EIGHT as *mut u8, 8)
    });
    lin::check_eq(&mut failed, b"the one-shot's read takes 8 bytes", r, 8);
    lin::check_eq(
        &mut failed,
        b"the count is exactly one expiration",
        unsafe { u64::from_ne_bytes(EIGHT) } as isize,
        1,
    );
    // Spent: what is left of a fired one-shot is nothing.
    lin::check_eq(
        &mut failed,
        b"gettime after the read succeeds",
        lin::timerfd_gettime(fd, &mut curr),
        0,
    );
    lin::check_true(
        &mut failed,
        b"a spent one-shot reports zeros",
        curr.value.tv_sec == 0 && curr.value.tv_nsec == 0,
    );

    // An interval timer: the expirations pile up while nobody reads, and
    // the read takes the whole pile in one u64.
    let arm = lin::Itimerspec {
        interval: lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        },
        value: lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        },
    };
    lin::check_eq(
        &mut failed,
        b"settime arming the interval succeeds",
        lin::timerfd_settime(fd, 0, &arm, core::ptr::null_mut()),
        0,
    );
    sleep_ms(100);
    let r = lin::read(fd, unsafe {
        core::slice::from_raw_parts_mut(&raw mut EIGHT as *mut u8, 8)
    });
    lin::check_eq(&mut failed, b"the interval read takes 8 bytes", r, 8);
    let count = unsafe { u64::from_ne_bytes(EIGHT) };
    lin::check_true(
        &mut failed,
        b"the pile is several expirations",
        count >= 2 && count <= 12,
    );

    // The disarm: a zero it_value is the man page's own spelling, and the
    // gettime after it agrees.
    let disarm = lin::Itimerspec::new();
    lin::check_eq(
        &mut failed,
        b"settime with a zero value disarms",
        lin::timerfd_settime(fd, 0, &disarm, core::ptr::null_mut()),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"gettime after the disarm succeeds",
        lin::timerfd_gettime(fd, &mut curr),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the disarmed timer reports zeros",
        curr.value.tv_sec == 0 && curr.value.tv_nsec == 0 && curr.interval.tv_sec == 0,
    );

    let _ = lin::close(pipe_fds[0]);
    let _ = lin::close(pipe_fds[1]);
    let _ = lin::close(nb);
    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-timerfd: panicked\n")
}
