//! t-clock-getres - what a clock says its own resolution is, and whether the
//! readings it delivers agree with that claim.
//!
//! The man page pins the shape: `clock_getres(clk_id, res)` fills a
//! `struct timespec` with the clock's resolution, answers `EINVAL` for an id
//! the kernel does not keep, and accepts a null `res` (a caller may ask only
//! whether the clock exists). What it does *not* say is what the number has to
//! be - and that is the half a kernel can get quietly wrong, because a caller
//! reads it to decide how coarse its own loop must be.
//!
//! Measured on the host (`build/scratch/batch/g.c`, `k.c`): the clocks that
//! read the free-running counter resolve to `1 ns` (`REALTIME`, `MONOTONIC`,
//! the two cputime clocks, `MONOTONIC_RAW`, `BOOTTIME`, `TAI` and the two
//! alarm clocks), and the two *coarse* clocks resolve to the tick (`1 ms`
//! there, because their internal tick is 1 ms) - and the readings match: 5000
//! back-to-back `MONOTONIC_COARSE` reads moved **0** times while `MONOTONIC`
//! moved 5000 times in 20 ns steps.
//!
//! This kernel answered one tick for *every* clock under a comment saying the
//! tick counter was the only time source there was, which had been overtaken:
//! `clock_gettime` interpolates the counter inside the tick. It also refused
//! `CLOCK_TAI` and the two alarm clocks - ids its own `clock_gettime` serves -
//! so one door called a number valid and the other called it malformed. Both
//! halves are pinned here, together with the reading-side claim that makes the
//! resolution mean something: a coarse clock's reading is a whole number of
//! the ticks it advertises, and an alarm clock reads like the clock it alarms
//! from (`CLOCK_BOOTTIME_ALARM` measured on the host as equal to
//! `CLOCK_BOOTTIME` to within the read).
//!
//! The ids are spelled, not taken from `libc`, because the claim is about the
//! numbers a real kernel accepts; the raw door is `libc::syscall`'s, since
//! rustix's `ClockId` vocabulary cannot name the gap or `999` and this file
//! needs the same door for the accepted ids.

use mpos_user_tests::Report;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;
const CLOCK_TAI: i32 = 11;

/// Every id a real kernel keeps, in Linux's numbering.
const IDS: [(i32, &str); 10] = [
    (CLOCK_REALTIME, "CLOCK_REALTIME"),
    (CLOCK_MONOTONIC, "CLOCK_MONOTONIC"),
    (CLOCK_PROCESS_CPUTIME_ID, "CLOCK_PROCESS_CPUTIME_ID"),
    (CLOCK_THREAD_CPUTIME_ID, "CLOCK_THREAD_CPUTIME_ID"),
    (CLOCK_MONOTONIC_RAW, "CLOCK_MONOTONIC_RAW"),
    (CLOCK_REALTIME_COARSE, "CLOCK_REALTIME_COARSE"),
    (CLOCK_MONOTONIC_COARSE, "CLOCK_MONOTONIC_COARSE"),
    (CLOCK_BOOTTIME, "CLOCK_BOOTTIME"),
    (CLOCK_REALTIME_ALARM, "CLOCK_REALTIME_ALARM"),
    (CLOCK_BOOTTIME_ALARM, "CLOCK_BOOTTIME_ALARM"),
];

/// One tick of this kernel's scheduler clock, in nanoseconds. Printed rather
/// than assumed: the claim below reads the coarse resolution back and compares
/// it against this, so a machine that changed `TICK_HZ` fails loudly.
const TICK_NS: i64 = 4_000_000;

/// A `timespec` in nanoseconds, and the resolution of one clock.
fn getres(id: i32) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a `timespec` this function owns, and an id from the table above.
    let rc = unsafe { libc::syscall(libc::SYS_clock_getres, id, &mut ts) } as i32;
    (rc == 0).then(|| ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64)
}

/// One clock's reading, in nanoseconds.
fn gettime(id: i32) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: as above.
    let rc = unsafe { libc::syscall(libc::SYS_clock_gettime, id, &mut ts) } as i32;
    (rc == 0).then(|| ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64)
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn main() {
    println!("t-clock-getres");
    let mut r = Report::new();

    // Every id the kernel serves has a resolution, and the number is a sane
    // `timespec` - this is the vocabulary claim, and the shape claim.
    for (id, name) in IDS {
        let res = getres(id);
        r.check(
            &format!("clock_getres({name}) answers a resolution"),
            res.is_some(),
        );
        if let Some(ns) = res {
            r.check(
                &format!("{name}'s resolution is inside one second ({ns} ns)"),
                (0..1_000_000_000).contains(&ns),
            );
        }
    }

    // The fine clocks are not the tick: this kernel interpolates the counter
    // inside the tick, so the smallest step is the counter's own. The bound is
    // deliberately loose (anything below a microsecond) because the two
    // machines' quanta differ - 1 ns on the first, `ceil(10^9/CNTFRQ)` on the
    // second - and both are far under it.
    let mono_res = getres(CLOCK_MONOTONIC).unwrap_or(-1);
    r.check(
        &format!("CLOCK_MONOTONIC resolves finer than a tick ({mono_res} ns)"),
        mono_res > 0 && mono_res < 1_000_000,
    );

    // The coarse clocks are the tick's own reading, and their advertised
    // resolution says exactly that. These three are the *machine's* claims: the
    // resolution is this kernel's tick (4 ms, `TICK_HZ = 250` on both machines),
    // and the readings are whole ticks because the handler takes them from the
    // tick counter. On a host the number is the host's own tick (1 ms here) and
    // its coarse reading carries a sub-tick offset, so a host run makes the
    // vocabulary claims above and nothing about these numbers.
    let mut moved = 0;
    if !mpos_user_tests::host_run() {
        for (id, name) in [
            (CLOCK_REALTIME_COARSE, "CLOCK_REALTIME_COARSE"),
            (CLOCK_MONOTONIC_COARSE, "CLOCK_MONOTONIC_COARSE"),
        ] {
            let res = getres(id).unwrap_or(-1);
            r.check(
                &format!("{name} resolves at the tick ({res} ns)"),
                res == TICK_NS,
            );
        }

        // And the coarse reading agrees with the claim: every one of them is a
        // whole number of the ticks it advertises. A coarse clock that
        // interpolated would answer a fraction here - measured before the fix,
        // 4827 distinct steps in 5000 reads.
        let quantum = getres(CLOCK_MONOTONIC_COARSE).unwrap_or(1).max(1);
        let mut fractions = 0;
        let mut prev = i64::MIN;
        let mut first = i64::MIN;
        let mut last = i64::MIN;
        for _ in 0..2_000 {
            if let Some(ns) = gettime(CLOCK_MONOTONIC_COARSE) {
                if ns % quantum != 0 {
                    fractions += 1;
                }
                if prev != i64::MIN && ns != prev {
                    moved += 1;
                }
                if first == i64::MIN {
                    first = ns;
                }
                last = ns;
                prev = ns;
            }
        }
        r.check(
            &format!(
                "a coarse reading is a whole number of its ticks ({fractions} fractions in 2000)"
            ),
            fractions == 0,
        );
        // The bound is the number of tick boundaries the loop itself crossed,
        // not a fixed count of steps. Two thousand reads spend however many
        // ticks the machine needs for them - one under a fast host, five on an
        // emulated aarch64 - and that number is a statement about the machine,
        // not about the clock. What the claim refuses is a coarse clock that
        // moves on nearly every read while advertising a tick's resolution:
        // measured before the fix, 1999 steps in 2000 reads.
        let spanned = if first == i64::MIN {
            0
        } else {
            (last - first) / quantum
        };
        r.check(
            &format!(
                "and the coarse clock moves only as often as its ticks do ({moved} steps over {spanned} ticks)"
            ),
            (moved as i64) <= spanned + 1,
        );
    }

    // The alarm clocks read like the clock they alarm from, not like the wall
    // clock: measured on the host, `BOOTTIME_ALARM` was `752 ns` from
    // `BOOTTIME` and `REALTIME_ALARM` `1313 ns` from `REALTIME`. The bound is a
    // millisecond - far under the ~1.8e18 ns that separates wall time from
    // boot time.
    let boot = gettime(CLOCK_BOOTTIME).unwrap_or(i64::MIN);
    let boot_alarm = gettime(CLOCK_BOOTTIME_ALARM).unwrap_or(i64::MIN);
    let realtime = gettime(CLOCK_REALTIME).unwrap_or(i64::MIN);
    let realtime_alarm = gettime(CLOCK_REALTIME_ALARM).unwrap_or(i64::MIN);
    r.check(
        &format!(
            "CLOCK_BOOTTIME_ALARM reads with CLOCK_BOOTTIME, not with the wall clock ({} ns apart)",
            boot_alarm - boot
        ),
        (boot_alarm - boot).abs() < 1_000_000,
    );
    r.check(
        &format!(
            "CLOCK_REALTIME_ALARM reads with CLOCK_REALTIME ({} ns apart)",
            realtime_alarm - realtime
        ),
        (realtime_alarm - realtime).abs() < 1_000_000,
    );

    // CLOCK_TAI is the wall clock here: this machine has no leap-second table,
    // and the host this was measured on had none either - `TAI` read `1162 ns`
    // from `REALTIME`. The claim is that the door serves it at all, which is
    // what the missing id set refused.
    //
    // The two readings are bracketed rather than taken side by side: a skew
    // between two clocks can be no larger than the interval the reads took, so
    // the wall clock is read again afterwards and the bracket is the bound.
    // A fixed 1 ms tolerance measured this as a failure on an emulated aarch64
    // (`1603792 ns apart`, where three syscalls are milliseconds of wall time),
    // which is the same mistake as a fixed step count: a claim about the host's
    // speed wearing a claim about the clock's face.
    let tai_wall_before = gettime(CLOCK_REALTIME).unwrap_or(i64::MIN);
    let tai = gettime(CLOCK_TAI);
    let tai_wall_after = gettime(CLOCK_REALTIME).unwrap_or(i64::MIN);
    r.check("CLOCK_TAI answers a reading", tai.is_some());
    if let Some(tai) = tai {
        let skew = (tai - tai_wall_before).abs();
        let spanned = (tai_wall_after - tai_wall_before).max(0);
        r.check(
            &format!(
                "and reads as the wall clock, this machine having no leap table ({skew} ns apart over a {spanned} ns read)"
            ),
            skew <= spanned + 1_000_000,
        );
    }

    // A clock that is not kept is EINVAL, whatever the destination: the gap at
    // 10 and the ids past 11 are the measured vocabulary, and a null `res` is
    // allowed (the caller asked only whether the clock exists).
    r.check(
        "clock_getres(10) - the gap in the id set - is EINVAL",
        getres(10).is_none() && last_errno() == libc::EINVAL,
    );
    // SAFETY: a null `res` is the documented "does this clock exist" form.
    let null_rc = unsafe { libc::syscall(libc::SYS_clock_getres, CLOCK_MONOTONIC, 0usize) } as i32;
    r.check("a null res is allowed, as Linux allows it", null_rc == 0);

    r.finish()
}
