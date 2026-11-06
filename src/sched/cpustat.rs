//! The machine's own counters: the CPU time charged in each mode, how many
//! tasks have been made, and how often the machine has switched between them.
//!
//! `/proc/stat` is the door these answer through, and it is the one file a
//! monitor reads before anything else: `vmstat`, `top`, `sar` and every
//! exporter take the `cpu` line's ten fields as *counters*, `btime` as the boot
//! instant, `processes` as the task-creation count and `ctxt` as the switch
//! count. Before this module the file printed the machine's **uptime in
//! seconds** as the `cpu` line's user field and had no `btime`, `processes`,
//! `procs_running` or `procs_blocked` line at all, so a reader did arithmetic on
//! a number that was not a counter.
//!
//! Measured on a current host (`build/scratch/stat/s.c`, `§6.100`) so the shape
//! here is Linux's and not a convenient one:
//!
//! * `processes` counts **threads as well as processes** - three `fork`s and
//!   three `pthread_create`s moved it `+3` and `+3`, because Linux increments
//!   `total_forks` in `copy_process`, which every `clone` goes through;
//! * `ctxt` counts context switches (`+12` for that same run's six tasks and
//!   their waits);
//! * `btime` is the boot instant in epoch seconds, `now - uptime` rounded to the
//!   second (`btime=1793942878` against `now=1793998793`, `uptime=55915.27`);
//! * `procs_running` is the runnable-thread count and `procs_blocked` the tasks
//!   in an *uninterruptible* wait, which is the same pair the load average
//!   already samples (`sched::loadavg`).
//!
//! # What this kernel does not count
//!
//! The `cpu` line has ten fields and this machine keeps three of them: user,
//! system and idle. The other seven - nice, iowait, irq, softirq, steal, guest,
//! guest_nice - are printed as zero because **this kernel has no separate
//! accounting for them**, not because they are empty: interrupt time is charged
//! to whatever the interrupt interrupted, exactly as the tick's own mode says,
//! and a uniprocessor with no steal or guest notion has nothing to report in the
//! others. Linux's own `irq`/`softirq` columns are time taken *out* of system
//! and charged on their own; adding that split would mean timing every handler,
//! which is a measurement this kernel does not make, and printing a plausible
//! zero there would be the "shape that looks finished while doing less" that
//! `AGENTS.md` §3 refuses. The gap is named in `docs/ROADMAP.md` with its
//! measurement.

use core::sync::atomic::{AtomicU64, Ordering};

/// One tick charged in user mode.
static USER_TICKS: AtomicU64 = AtomicU64::new(0);
/// One tick charged in kernel mode, the machine's own work included.
static SYSTEM_TICKS: AtomicU64 = AtomicU64::new(0);
/// One tick where no task was running: the idle loop's own time.
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);
/// Tasks created since boot - Linux's `processes`, which counts threads too.
static TASKS_MADE: AtomicU64 = AtomicU64::new(0);
/// Context switches since boot.
static SWITCHES: AtomicU64 = AtomicU64::new(0);

/// One tick, charged to `in_user` if a task was running and to idle if not.
///
/// Called from the tick interrupt, so: no allocation, no walking, three adds at
/// most - the discipline `sched::loadavg`'s sampling already keeps.
pub fn charge(in_user: bool, running: bool) {
    let counter = if !running {
        &IDLE_TICKS
    } else if in_user {
        &USER_TICKS
    } else {
        &SYSTEM_TICKS
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A task slot was taken: one more task exists (Linux counts threads here, and
/// so does this - `alloc_slot` is the one door a task enters through).
pub fn count_task() {
    TASKS_MADE.fetch_add(1, Ordering::Relaxed);
}

/// The machine switched from one context to another.
pub fn count_switch() {
    SWITCHES.fetch_add(1, Ordering::Relaxed);
}

/// The counters, as one reading - so a file that prints several lines takes
/// them from one moment rather than from four separate loads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Counters {
    /// Ticks charged in user mode.
    pub user: u64,
    /// Ticks charged in kernel mode.
    pub system: u64,
    /// Ticks where nothing was running.
    pub idle: u64,
    /// Tasks created since boot.
    pub tasks: u64,
    /// Context switches since boot.
    pub switches: u64,
}

/// Read every counter.
pub fn counters() -> Counters {
    Counters {
        user: USER_TICKS.load(Ordering::Relaxed),
        system: SYSTEM_TICKS.load(Ordering::Relaxed),
        idle: IDLE_TICKS.load(Ordering::Relaxed),
        tasks: TASKS_MADE.load(Ordering::Relaxed),
        switches: SWITCHES.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    for counter in [
        &USER_TICKS,
        &SYSTEM_TICKS,
        &IDLE_TICKS,
        &TASKS_MADE,
        &SWITCHES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one property the file depends on: a tick lands in exactly one of the
    /// three CPU columns, so the sum of the columns is the ticks the machine has
    /// had, and a tick with no task running is idle rather than charged to
    /// whoever happened to hold the CPU last.
    #[test]
    fn a_tick_lands_in_one_mode_and_idle_means_nobody_was_running() {
        let _lock = crate::sched::test_lock::take();
        reset_for_test();
        charge(false, true); // system, a task running in kernel mode
        charge(true, true); // user
        charge(true, false); // idle: the user flag cannot make nobody a caller
        let got = counters();
        assert_eq!((got.user, got.system, got.idle), (1, 1, 1));
        assert_eq!(got.user + got.system + got.idle, 3);
    }

    /// Creation and switching are counted where they happen, and nothing else
    /// reads as either.
    #[test]
    fn tasks_and_switches_are_counted_apart_from_the_cpu_columns() {
        let _lock = crate::sched::test_lock::take();
        reset_for_test();
        count_task();
        count_task();
        count_switch();
        let got = counters();
        assert_eq!((got.tasks, got.switches), (2, 1));
        assert_eq!(got.user + got.system + got.idle, 0);
    }
}
