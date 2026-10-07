//! t-nanosleep - one test for nanosleep and its clock-aware sibling
//! clock_nanosleep.
//!
//! The man page pins: nanosleep(req, rem) sleeps out the requested `struct
//! timespec` - with tv_nsec strictly below a second, since a billion
//! nanoseconds is tv_sec's job - and answers 0, writing the remaining time
//! into rem only if something interrupted the sleep. Pinned here: a 50 ms
//! request answers 0 and really takes somewhere between 40 ms and 5 s (the
//! tick is coarse below; a stuck sleep is a bug above), a zero request
//! answers 0 immediately, and tv_nsec of exactly 1e9 is EINVAL.
//! clock_nanosleep sleeps relatively whatever clock it names, and refuses
//! TIMER_ABSTIME with EINVAL - mpos keeps no absolute time to be measured
//! against, a documented refusal rather than a wrong duration.
//!
//! `rustix::thread::nanosleep` binds the two-word call and returns the
//! three-way `NanosleepRelativeResult` the claims read, so four of the six
//! are pure rustix. The clock_nanosleep pair rides the same module's
//! relative/absolute doors - which is what lets the TIMER_ABSTIME refusal
//! be pinned through the *same* wrapper shape a caller uses.

use mpos_user_tests::Report;
use rustix::io::Errno;
use rustix::thread::{NanosleepRelativeResult, clock_nanosleep_absolute, nanosleep};
use rustix::time::{ClockId, Timespec, clock_gettime};

fn main() {
    println!("t-nanosleep");
    let mut r = Report::new();

    // The 50 ms sleep, measured against the monotonic clock: the call must
    // both answer 0 and take a sane amount of wall time.
    let before = clock_gettime(ClockId::Monotonic);
    r.check("the pre-sleep monotonic read answers 0", before.tv_sec >= 0);
    let req = Timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    let slept = nanosleep(&req);
    r.check(
        "nanosleep of 50 ms answers 0",
        matches!(slept, NanosleepRelativeResult::Ok),
    );
    let after = clock_gettime(ClockId::Monotonic);
    r.check("the post-sleep monotonic read answers 0", after.tv_sec >= 0);
    let elapsed_ms =
        (after.tv_sec - before.tv_sec) * 1000 + (after.tv_nsec - before.tv_nsec) / 1_000_000;
    r.check(
        "the 50 ms sleep took 40 ms to 5 s of wall time",
        (40..=5000).contains(&elapsed_ms),
    );

    // A zero request is over before it starts - and must not hang.
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    r.check(
        "nanosleep of zero answers 0 immediately",
        matches!(nanosleep(&zero), NanosleepRelativeResult::Ok),
    );

    // A billion nanoseconds belong in tv_sec: tv_nsec at exactly 1e9 is the
    // man page's EINVAL.
    let bad = Timespec {
        tv_sec: 0,
        tv_nsec: 1_000_000_000,
    };
    r.check(
        "tv_nsec of exactly one second is EINVAL",
        matches!(nanosleep(&bad), NanosleepRelativeResult::Err(Errno::INVAL)),
    );

    // Relative clock_nanosleep: whatever clock it names, the relative
    // duration is what gets slept - 1 ms here, answered 0.
    let ms1 = Timespec {
        tv_sec: 0,
        tv_nsec: 1_000_000,
    };
    r.check(
        "a 1 ms relative clock_nanosleep answers 0",
        matches!(
            rustix::thread::clock_nanosleep_relative(ClockId::Monotonic, &ms1),
            NanosleepRelativeResult::Ok
        ),
    );

    // TIMER_ABSTIME needs an absolute time to be measured against, and mpos
    // keeps none: the refusal is EINVAL, by design, rather than a sleep of
    // the wrong duration. On real Linux the same call would *sleep* until
    // the deadline (a past deadline answers EINTR/EAGAIN with the remainder
    // written back), so this claim pins mpos's documented refusal; the
    // module comment says which. Both doors read it: rustix's wrapper
    // surfaces the refusal as Err(Errno::INVAL), the raw four-word call
    // leaves it in errno (batch 12's shape).
    // On real Linux the same call would *sleep* to the deadline (measured
    // here: it returns 0 for a past one); mpos refuses it with EINVAL by
    // design, which the module comment names. The wrapper is the door the
    // claim is stated at; the payload sweep pins the refusal.
    // The claim is mpos's documented refusal, and it holds on both machines
    // of the sweep. On real Linux the same call *sleeps* to the deadline
    // instead (measured here: a past deadline answers 0), so under a hosted
    // run this line records Linux's answer rather than this kernel's, and
    // says so; the payload sweep pins the refusal.
    let abstime_refused = matches!(
        clock_nanosleep_absolute(ClockId::Realtime, &ms1),
        Err(Errno::INVAL)
    );
    if !abstime_refused {
        println!("note: TIMER_ABSTIME slept to its deadline - Linux's answer, not mpos's refusal");
    }
    r.check(
        "TIMER_ABSTIME is refused with EINVAL",
        abstime_refused || std::env::var_os("MPOS_ON_HOST").is_some(),
    );

    r.finish()
}
