//! `/proc/<pid>/stat`'s `starttime` (field 22) - when the task came into being,
//! in the `USER_HZ` ticks the `btime` beside it is counted in.
//!
//! Measured on a current host (`build/scratch/starttime/p.c`, Linux 7.2.14):
//! two children forked a second apart read field 22 one hundred ticks apart,
//! and the reading process's own field put its age at `0.090 s` against an
//! `uptime` of `63431.94 s` - so the field is the *creation* instant in
//! centiseconds, which is what `ps` subtracts from `btime` to print an age.
//!
//! The claims here are all movements or agreements rather than a constant: this
//! kernel prints a tick count, the same number through two paths, and a later
//! child a later count. A kernel that printed a literal `0` - which this one did
//! until `docs/notes/` §6.104 - fails the last two.

use mpos_user_tests::Report;
use std::io::Write;

/// `sysconf(_SC_CLK_TCK)`: the unit `starttime` is in, and the number a reader
/// divides by. Linux's `USER_HZ` is a compile-time 100; this kernel reports the
/// same unit by truncating the nanosecond record it keeps.
const USER_HZ: i64 = 100;

/// The field the reader asks for, counted the way `/proc/<pid>/stat` counts:
/// the command name may hold spaces and parentheses, so the fields start after
/// the last `)` and field `N` is the `N - 3`rd token after it.
fn stat_field(text: &str, field: usize) -> Option<i64> {
    let after = &text[text.rfind(')')? + 1..];
    let mut tokens = after.split_whitespace();
    // The token right after `)` is field 3 (`state`).
    let mut index = 3;
    loop {
        let token = tokens.next()?;
        if index == field {
            return token.parse().ok();
        }
        index += 1;
    }
}

fn read_stat(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn starttime_of(path: &str) -> Option<i64> {
    stat_field(&read_stat(path)?, 22)
}

/// `/proc/uptime`'s first number, in milliseconds - the age of the machine,
/// which is what a zero `starttime` would make every process's age.
fn uptime_ms() -> Option<i64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let first = text.split_whitespace().next()?;
    let seconds: f64 = first.parse().ok()?;
    Some((seconds * 1000.0) as i64)
}

/// A child that stays alive long enough to be read, and its pid.
fn spawn_sleeper() -> Option<i32> {
    // SAFETY: `fork` in a single-threaded test process: the child touches
    // nothing but `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return None;
    }
    if pid == 0 {
        // SAFETY: plain `nanosleep`, then `_exit` - no destructor, no allocator.
        unsafe {
            let spec = libc::timespec {
                tv_sec: 5,
                tv_nsec: 0,
            };
            libc::nanosleep(&spec, std::ptr::null_mut());
            libc::_exit(0);
        }
    }
    Some(pid as i32)
}

fn reap(pid: i32) {
    // SAFETY: the pid is this process's own child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    let _ = std::io::stdout().flush();
}

fn main() {
    let mut r = Report::new();

    let own = starttime_of("/proc/self/stat");
    r.check(
        &format!("stat's starttime is a tick count, not a literal zero ({own:?})"),
        own.is_some_and(|ticks| ticks > 0),
    );

    // The same number through the directory path: `/proc/self/stat` and
    // `/proc/<pid>/stat` must be one record, or a monitor reading a pid it was
    // handed gets a different answer than the process reads about itself.
    let via_dir = starttime_of(&format!("/proc/{}/stat", std::process::id()));
    r.check(
        &format!(
            "the task's own file and its /proc/<pid>/stat agree ({own:?} against {via_dir:?})"
        ),
        own.is_some() && own == via_dir,
    );

    // A field that moved while the task runs would be an age, not a birth.
    let first = starttime_of("/proc/self/stat");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let second = starttime_of("/proc/self/stat");
    r.check(
        &format!("the field does not move while the task runs ({first:?} against {second:?})"),
        first.is_some() && first == second,
    );

    // Two children, one wait apart. The difference is the pin: a literal zero
    // makes it zero, whatever the machine's age is.
    let soon = spawn_sleeper();
    let born_at = uptime_ms();
    let soon_time = soon.and_then(|pid| starttime_of(&format!("/proc/{pid}/stat")));
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    let late = spawn_sleeper();
    let late_time = late.and_then(|pid| starttime_of(&format!("/proc/{pid}/stat")));
    let late_at = uptime_ms();

    let delta = match (soon_time, late_time) {
        (Some(early), Some(later)) => Some(later - early),
        _ => None,
    };
    r.check(
        &format!("a child created later carries a later starttime ({delta:?} ticks)"),
        delta.is_some_and(|ticks| ticks > 0),
    );

    // ... and the difference is the wait itself, in centiseconds. 1.5 s of
    // `nanosleep` plus the fork of the second child, so 100 ticks and a little;
    // the window is the measurement's own error, not a value to hold.
    let elapsed = match (born_at, late_at) {
        (Some(before), Some(after)) => Some(after - before),
        _ => None,
    };
    let agrees = matches!((delta, elapsed), (Some(ticks), Some(ms))
        if (ticks * (1000 / USER_HZ) - ms).abs() <= 60);
    r.check(
        &format!("and the difference is the wait, a tick per centisecond ({delta:?} ticks against {elapsed:?} ms)"),
        agrees,
    );

    // The age a reader would compute: `uptime - starttime/USER_HZ`. This run
    // began after the boot, so the two must not be the same number.
    let age_ms = match (own, uptime_ms()) {
        (Some(ticks), Some(up)) => Some(up - ticks * (1000 / USER_HZ)),
        _ => None,
    };
    r.check(
        &format!(
            "and an age computed from it is younger than the machine ({age_ms:?} ms of {0:?} ms)",
            uptime_ms()
        ),
        matches!((age_ms, uptime_ms()), (Some(age), Some(up)) if age >= 0 && age < up),
    );

    for pid in [soon, late].into_iter().flatten() {
        reap(pid);
    }

    r.finish();
}
