//! `CLOCK_PROCESS_CPUTIME_ID` and `CLOCK_THREAD_CPUTIME_ID` are CPU time, not
//! clocks that run with the machine.
//!
//! Measured on a current host (`build/scratch/cpuclock/p.c`): a process 46.4 ms
//! into its life read `cpu=46409 us` while `RUSAGE_SELF` answered `46410 us`,
//! the clock advanced **9 us** across a 300 ms sleep, and a worker thread that
//! burned 298 ms of CPU while the main thread burned 450 ms read 298 ms on its
//! own `CLOCK_THREAD_CPUTIME_ID` and 753 ms on the process's. Both clocks
//! resolve to `1 ns` there because Linux charges CPU in nanoseconds.
//!
//! This kernel answered both ids with `since_boot_nanos` - machine uptime - so
//! a 1.1 s boot read `cpu=1113219 us`, the "CPU time" advanced 301 ms across a
//! 300 ms sleep, and two threads of one process read the same number. That is
//! the §4.2 success-for-a-thing-not-done with a plausible-looking number: the
//! shape of a `timespec` and none of the content. The counters it reads now are
//! the ones `getrusage` and `/proc/<pid>/stat` answer from (tick granularity,
//! which `clock_getres` now states for these two ids - `§6.99`).

use mpos_user_tests::{Report, Rusage, Timeval, host_run};

const TICK_NS: i64 = 4_000_000;

fn clock_ns(id: libc::clockid_t) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(id, &mut ts) } != 0 {
        return None;
    }
    Some(ts.tv_sec * 1_000_000_000 + ts.tv_nsec)
}

fn clock_res(id: libc::clockid_t) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_getres(id, &mut ts) } != 0 {
        return None;
    }
    Some(ts.tv_sec * 1_000_000_000 + ts.tv_nsec)
}

/// The process's own CPU, the way `RUSAGE_SELF` reports it - user plus system,
/// no children.
fn rusage_self_us() -> i64 {
    let mut ru = Rusage {
        ru_utime: Timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_stime: Timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        counters: [0; 14],
    };
    let raw = &mut ru as *mut Rusage as *mut libc::rusage;
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, raw) } != 0 {
        return -1;
    }
    ru.ru_utime.micros() + ru.ru_stime.micros()
}

/// Burn CPU for roughly `usec` microseconds of wall time, timed by the
/// monotonic clock so a slow machine still stops.
fn burn(usec: i64) {
    let start = clock_ns(libc::CLOCK_MONOTONIC).unwrap_or(0);
    let mut x: u64 = 0;
    loop {
        for i in 0..100_000u64 {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        match clock_ns(libc::CLOCK_MONOTONIC) {
            Some(now) if now - start >= usec * 1000 => break,
            _ => {}
        }
    }
}

fn claim_the_two_cpu_clocks(r: &mut Report) {
    let process_before = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
    let rusage_before = rusage_self_us();
    match (process_before, rusage_before) {
        (Some(process), rusage) if rusage >= 0 => {
            let process_us = process / 1000;
            let apart = (process_us - rusage).abs();
            r.check(
                &format!(
                    "CLOCK_PROCESS_CPUTIME_ID and RUSAGE_SELF agree about this process's CPU \
                     ({process_us} us against {rusage} us, {apart} us apart)"
                ),
                apart <= 2 * TICK_NS / 1000,
            );
        }
        _ => r.check("CLOCK_PROCESS_CPUTIME_ID answers at all", false),
    }

    // A sleeping process spends no CPU. The unfixed kernel advanced by exactly
    // the sleep, which is what makes this the claim that separates a clock from
    // a counter of elapsed time.
    let before = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
    let grew_us = match (before, after) {
        (Some(a), Some(b)) => (b - a) / 1000,
        _ => i64::MAX,
    };
    r.check(
        &format!(
            "and it does not run while the process sleeps (grew {grew_us} us over a 300 ms sleep)"
        ),
        (0..=10_000).contains(&grew_us),
    );

    // ...and it does move when the thread does work.
    let before = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
    burn(300_000);
    let burned_us = match (before, clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID)) {
        (Some(a), Some(b)) => (b - a) / 1000,
        _ => -1,
    };
    r.check(
        &format!("and it moves when this thread burns CPU ({burned_us} us for a 300 ms burn)"),
        (TICK_NS / 1000..=900_000).contains(&burned_us),
    );

    // A sibling's CPU belongs to the process, not to this thread. The worker
    // burns ~200 ms; this thread then reads both clocks without burning.
    let thread_before = clock_ns(libc::CLOCK_THREAD_CPUTIME_ID);
    let process_before = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID);
    let worker = std::thread::spawn(|| burn(200_000));
    let _ = worker.join();
    let thread_grew = match (thread_before, clock_ns(libc::CLOCK_THREAD_CPUTIME_ID)) {
        (Some(a), Some(b)) => (b - a) / 1000,
        _ => i64::MAX,
    };
    let process_grew = match (process_before, clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID)) {
        (Some(a), Some(b)) => (b - a) / 1000,
        _ => 0,
    };
    r.check(
        &format!(
            "a sibling's CPU is the process's, not this thread's (this thread grew {thread_grew} us \
             while the process grew {process_grew} us over the sibling's 200 ms burn)"
        ),
        thread_grew < 100_000 && process_grew >= 100_000,
    );

    // The resolution has to be the step the counter actually moves in. Linux
    // charges CPU in nanoseconds and answers `1 ns`; this kernel charges it per
    // tick, so the honest answer here is the tick - and answering `1 ns` while
    // the value steps by 4 ms is §6.97's defect in the other direction.
    let resolution = clock_res(libc::CLOCK_PROCESS_CPUTIME_ID);
    let detail = match resolution {
        Some(ns) => ns,
        None => -1,
    };
    if host_run() {
        r.check(
            &format!("clock_getres states the granularity of the CPU counters ({detail} ns)"),
            (0..=1_000).contains(&detail),
        );
    } else {
        r.check(
            &format!("clock_getres states the granularity of the CPU counters ({detail} ns)"),
            detail == TICK_NS,
        );
    }
}

fn main() {
    let mut r = Report::new();
    claim_the_two_cpu_clocks(&mut r);
    r.finish();
}
