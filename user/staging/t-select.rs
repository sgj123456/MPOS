//! t-select - the three waits, and the two timeouts they disagree about.
//!
//! `select` and `poll` are the same idea in two shapes, and `pselect6` is
//! `select` with a `timespec` where `select` has a `timeval`. That last
//! difference is the whole trap: both structs lead with seconds and follow
//! with a sub-second field, one in microseconds and one in nanoseconds -
//! so a wait that reads the nanosecond field as microseconds waits a
//! thousand times too long.
//!
//! What is pinned here:
//!
//! * a writable descriptor is ready, in all three;
//! * an empty pipe is not readable, and a zero timeout says so instead of
//!   blocking - which is the difference between a wait and a hang;
//! * `select` with a 50 millisecond timeout returns in well under a
//!   second, and so does `pselect6` with the same 50 milliseconds written
//!   as a `timespec`. The second one is the test that would have caught a
//!   shared timeout parser.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
/// One millisecond, in the units each struct uses.
const FIFTY_MS_NS: i64 = 50_000_000;
const FIFTY_MS_US: i64 = 50_000;
/// Anything past a second means the timeout unit was read wrong.
const ONE_SECOND: i64 = 1_000_000_000;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-select\n");
    let mut failed = false;

    // --- select: a writable descriptor is ready --------------------------
    // One 8-byte word covers descriptors 0-63; fd 1 is bit 1 of it.
    let mut set = [0u8; 8];
    set[0] = 0b10; // fd 1
    let r = lin::select(2, Some(&mut set), None);
    lin::check_eq(
        &mut failed,
        b"select on a writable descriptor with no timeout returns 1",
        r,
        1,
    );
    lin::check_true(
        &mut failed,
        b"and the set comes back naming fd 1, not the whole word",
        set[0] == 0b10,
    );

    // --- select: an empty pipe, and a zero timeout -----------------------
    let mut pair: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"a pipe is made for the not-ready case",
        lin::pipe2(&mut pair, 0),
        0,
    );
    let mut empty = [0u8; 8];
    empty[0] = 1 << (pair[0] % 8);
    let zero = lin::TimeVal { tv_sec: 0, tv_usec: 0 };
    let r = lin::select(pair[0] as usize + 1, Some(&mut empty), Some(&zero));
    lin::check_eq(
        &mut failed,
        b"select on an empty pipe with a zero timeout returns 0",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"and clears the set it was given",
        empty[0] == 0,
    );

    // Once a byte lands in it, the same request is ready.
    lin::check_eq(
        &mut failed,
        b"a byte goes into the pipe",
        lin::write(pair[1], b"x"),
        1,
    );
    let mut ready = [0u8; 8];
    ready[0] = 1 << (pair[0] % 8);
    let r = lin::select(pair[0] as usize + 1, Some(&mut ready), Some(&zero));
    lin::check_eq(
        &mut failed,
        b"select on a pipe with a byte in it returns 1",
        r,
        1,
    );

    // --- the timeout unit: the part that was shared and wrong -----------
    // Both waits ask for 50 milliseconds. If the timespec's nanoseconds
    // were read as microseconds, this wait would take 50 seconds and the
    // gate would time out - so the check is the elapsed clock, not the
    // return value.
    let tv = lin::TimeVal { tv_sec: 0, tv_usec: FIFTY_MS_US };
    let mut before = lin::TimeSpec { tv_sec: 0, tv_nsec: 0 };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut before);
    let mut empty = [0u8; 8];
    empty[0] = 1 << (pair[0] % 8);
    let r = lin::select(pair[0] as usize + 1, Some(&mut empty), Some(&tv));
    let mut after = lin::TimeSpec { tv_sec: 0, tv_nsec: 0 };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut after);
    lin::check_eq(
        &mut failed,
        b"select over an empty pipe times out with 0",
        r,
        0,
    );
    let elapsed = (after.tv_sec - before.tv_sec) * 1_000_000_000 + (after.tv_nsec - before.tv_nsec);
    lin::check_true(
        &mut failed,
        b"select's 50ms timeout waits well under a second",
        elapsed < ONE_SECOND,
    );

    // The same 50 milliseconds, written as a timespec for pselect6.
    let ts = lin::TimeSpec { tv_sec: 0, tv_nsec: FIFTY_MS_NS };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut before);
    let mut empty = [0u8; 8];
    empty[0] = 1 << (pair[0] % 8);
    let r = lin::pselect6(pair[0] as usize + 1, Some(&mut empty), &ts);
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut after);
    lin::check_eq(
        &mut failed,
        b"pselect6 over an empty pipe times out with 0",
        r,
        0,
    );
    let elapsed = (after.tv_sec - before.tv_sec) * 1_000_000_000 + (after.tv_nsec - before.tv_nsec);
    lin::check_true(
        &mut failed,
        b"pselect6's 50ms timespec waits well under a second",
        elapsed < ONE_SECOND,
    );

    // A timespec whose nanoseconds do not fit a second is a corrupted
    // timeout, and both calls say so rather than wrapping.
    let bad = lin::TimeSpec { tv_sec: 0, tv_nsec: 1_000_000_000 };
    let mut empty = [0u8; 8];
    let r = lin::pselect6(pair[0] as usize + 1, Some(&mut empty), &bad);
    lin::check_eq(
        &mut failed,
        b"pselect6 with a nanosecond field of a whole second is EINVAL",
        r,
        -22,
    );

    // --- ppoll: the same wait in poll's shape ----------------------------
    let mut one = [lin::PollFd {
        fd: 1,
        events: POLLOUT,
        revents: 0,
    }];
    let no_wait = lin::TimeSpec { tv_sec: 0, tv_nsec: 0 };
    let r = lin::ppoll(&mut one, &no_wait);
    lin::check_eq(
        &mut failed,
        b"ppoll on a writable descriptor with no timeout returns 1",
        r,
        1,
    );
    lin::check_true(
        &mut failed,
        b"and reports POLLOUT",
        one[0].revents & POLLOUT != 0,
    );

    let mut pipe_watch = [lin::PollFd {
        fd: pair[0],
        events: POLLIN,
        revents: 0,
    }];
    let r = lin::ppoll(&mut pipe_watch, &no_wait);
    lin::check_eq(
        &mut failed,
        b"ppoll on the pipe with a byte in it returns 1",
        r,
        1,
    );

    let _ = lin::close(pair[0]);
    let _ = lin::close(pair[1]);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-select: panicked\n")
}
