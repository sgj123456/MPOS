//! The load average: Linux's `avenrun`, and the two doors that report it.
//!
//! `/proc/loadavg` printed `0.00 0.00 0.00 1/<procs>` on every machine, while
//! `docs/uapi/03-flag-vocabularies.md` listed the file under "served as the
//! neutral renderings" and `sysinfo` left `loads[0..3]` zero. A load average of
//! zero is not a neutral rendering: it is the statement that nothing has wanted
//! the CPU since boot, and every tool built on it - `uptime`, `top`,
//! node exporters, a Makefile sizing its job count - reads that as a fact.
//! `AGENTS.md` §3 rule 5's "a shape that looks finished while doing less" is
//! exactly this, so the numbers are computed here instead of printed.
//!
//! **Measured first, on Linux 7.2.14** (musl, `build/scratch/loadavg/p.c`):
//! an idle 16-CPU host printed `1.85 2.04 2.21 3/1312`, and forty reads 100 ms
//! apart saw the three averages change only at 5-second boundaries - `1.85
//! 2.04 2.21` became `1.94 2.05 2.22`, then `2.27 2.12 2.24` five seconds
//! later - while the runnable/threads field changed between every pair of
//! reads. `sysinfo()` in the same window answered `loads = 127456 134624
//! 145376`, and `127456 / 65536 = 1.9449`: one fixed-point `avenrun` value,
//! reported two ways (`loadavg` as integer and two decimals, `sysinfo` shifted
//! into its 16-bit fractional unit). Four spinners for six seconds moved the
//! one-minute figure from `1.94` to `2.27` at the next boundary, and moved
//! `sysinfo.loads[0]` to `148768` in the same instant - the two doors share
//! one sample, so they must never disagree.
//!
//! ```text
//! kernel/sched/loadavg.c:  FSHIFT 11, FIXED_1 (1 << 11), EXP_1 1884,
//!                          EXP_5 2014, EXP_15 2037, LOAD_FREQ 5 * HZ
//! calc_load(load, exp, active):  new = load * exp + active * (FIXED_1 - exp)
//!                                if active >= load: new += FIXED_1 - 1
//!                                new / FIXED_1
//! ```
//!
//! The arithmetic is host-compiled and unit-tested (it is integers, not a
//! machine); only the sampling hook - one tick in every five seconds' worth -
//! lives in the tick handler. `EXP_1`/`EXP_5`/`EXP_15` are `1/exp(5s/60s)`,
//! `1/exp(5s/300s)` and `1/exp(5s/900s)` in `FIXED_1` units, and the rounding
//! term is Linux's own: it rounds *up* when `active >= load`, which is what
//! makes a host that has just started a spinner show a number larger than the
//! one before it rather than the same one truncated (measured above: `1.94` ->
//! `2.27`, not `2.26`).

use core::sync::atomic::{AtomicU64, Ordering};

/// Linux's `FSHIFT`: eleven fractional bits in every average below.
pub const FSHIFT: u32 = 11;

/// `FIXED_1` - one whole unit of load, in [`FSHIFT`] fractional bits.
pub const FIXED_1: u64 = 1 << FSHIFT;

/// The one-minute decay constant, `1/exp(5s/60s)`, in `FIXED_1` units.
pub const EXP_1: u64 = 1884;
/// The five-minute decay constant, `1/exp(5s/300s)`.
pub const EXP_5: u64 = 2014;
/// The fifteen-minute decay constant, `1/exp(5s/900s)`.
pub const EXP_15: u64 = 2037;

/// `SI_LOAD_SHIFT`: `struct sysinfo`'s `loads[]` are `avenrun` shifted here.
pub const SI_LOAD_SHIFT: u32 = 16;

/// Linux's `LOAD_FREQ`, in seconds: `calc_global_load_tick` samples the
/// runnable count once every five seconds, not once per tick.
pub const LOAD_FREQ_SECS: u64 = 5;

/// One `calc_load` step: take `load` one sample closer to `active`, both in
/// [`FSHIFT`] fixed point, with Linux's own rounding term.
///
/// `active` is the instantaneous runnable count *in fixed point* -
/// `nr_active() * FIXED_1` in Linux's `calc_global_load` - so a machine with
/// one runnable task and no history answers `FIXED_1 * (FIXED_1 - EXP_1) /
/// FIXED_1 + rounding` rather than a number a thousandth of that.
pub fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut new = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        new += FIXED_1 - 1;
    }
    new / FIXED_1
}

/// The three averages in fixed point, updated on the sampling tick and read by
/// both doors. `const`-constructible so it can be a `static`.
pub struct Averages {
    one: AtomicU64,
    five: AtomicU64,
    fifteen: AtomicU64,
}

impl Averages {
    pub const fn new() -> Averages {
        Averages {
            one: AtomicU64::new(0),
            five: AtomicU64::new(0),
            fifteen: AtomicU64::new(0),
        }
    }

    /// The three averages in fixed point, in `1`/`5`/`15` minute order.
    pub fn get(&self) -> [u64; 3] {
        [
            self.one.load(Ordering::Relaxed),
            self.five.load(Ordering::Relaxed),
            self.fifteen.load(Ordering::Relaxed),
        ]
    }

    /// One sample: `active` is the number of tasks that were runnable (or
    /// uninterruptible) at the sampling tick.
    ///
    /// Three multiplies and a divide - interrupt context work, bounded, no
    /// allocation, which is why the sample is taken from the tick handler
    /// rather than from a task.
    pub fn update(&self, active: u64) {
        let active = active.saturating_mul(FIXED_1);
        let one = calc_load(self.one.load(Ordering::Relaxed), EXP_1, active);
        let five = calc_load(self.five.load(Ordering::Relaxed), EXP_5, active);
        let fifteen = calc_load(self.fifteen.load(Ordering::Relaxed), EXP_15, active);
        self.one.store(one, Ordering::Relaxed);
        self.five.store(five, Ordering::Relaxed);
        self.fifteen.store(fifteen, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn seed(&self, values: [u64; 3]) {
        self.one.store(values[0], Ordering::Relaxed);
        self.five.store(values[1], Ordering::Relaxed);
        self.fifteen.store(values[2], Ordering::Relaxed);
    }
}

/// The kernel's one `avenrun`, the value both `/proc/loadavg` and
/// `sysinfo.loads` report.
pub static AVENRUN: Averages = Averages::new();

/// Ticks counted since the last sample.
static SINCE_SAMPLE: AtomicU64 = AtomicU64::new(0);

/// Count one tick; answer whether this tick is a sampling tick.
///
/// `TICK_HZ` is the machine's, so it is passed in rather than assumed: the
/// sample interval is `LOAD_FREQ * HZ` ticks, and a port with a different tick
/// rate must still sample every five seconds.
pub fn sample_tick(tick_hz: u32) -> bool {
    let period = LOAD_FREQ_SECS * tick_hz as u64;
    let seen = SINCE_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
    if period != 0 && seen % period == 0 {
        SINCE_SAMPLE.store(0, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// The integer and hundredths halves of one average, as `/proc/loadavg` prints
/// them.
///
/// Linux's `loadavg_proc_show` adds `FIXED_1/200` before splitting - half of
/// the last printed hundredth, so the two decimals round rather than truncate.
/// That is why a host answering `sysinfo.loads = 127456` (`avenrun = 3983`,
/// `3983 / 2048 = 1.945`) has the file print `1.94`: `3983 + 10` gives integer
/// `1` and hundredths `94`.
pub fn hundredths(load: u64) -> (u64, u64) {
    let scaled = load + FIXED_1 / 200;
    (scaled >> FSHIFT, ((scaled & (FIXED_1 - 1)) * 100) >> FSHIFT)
}

/// One average in `struct sysinfo`'s spelling: `get_avenrun(loads, 0,
/// SI_LOAD_SHIFT - FSHIFT)` shifts it up by five bits.
pub fn sysinfo_load(load: u64) -> u64 {
    load << (SI_LOAD_SHIFT - FSHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One runnable task, no history: Linux's first sample is
    /// `(0 + 1 * FIXED_1 * (FIXED_1 - EXP_1) + (FIXED_1 - 1)) / FIXED_1`.
    #[test]
    fn the_first_sample_of_one_runnable_task_is_one_tasks_worth() {
        // (0 + 2048 * 164 + 2047) / 2048 = 164.99, and floors to 164 - 0.08
        // of load, which is the `0.08` the file prints for one busy task.
        assert_eq!(calc_load(0, EXP_1, FIXED_1), 164);
        assert_eq!(hundredths(164), (0, 8));
    }

    /// A steady one runnable task converges on exactly 1.00, and stays there.
    #[test]
    fn a_steady_load_is_its_own_fixed_point() {
        assert_eq!(calc_load(2048, EXP_1, FIXED_1), 2048);
        assert_eq!(hundredths(2048), (1, 0));
        assert_eq!(sysinfo_load(2048), 65536);
    }

    /// An idle machine decays by the fifteen-minute ... by the one-minute
    /// constant, and never below zero.
    #[test]
    fn an_idle_machine_decays_by_the_one_minute_constant() {
        assert_eq!(calc_load(2048, EXP_1, 0), 1884);
        assert_eq!(calc_load(0, EXP_1, 0), 0);
    }

    /// The host measured `sysinfo.loads = 127456` beside `loadavg`'s `1.94`
    /// (`build/scratch/loadavg/p.c`); both come out of one fixed-point value.
    #[test]
    fn the_two_doors_report_the_same_fixed_point_value() {
        let avenrun = 3983;
        assert_eq!(hundredths(avenrun), (1, 94));
        assert_eq!(sysinfo_load(avenrun), 127_456);
        assert_eq!(sysinfo_load(avenrun) / 65536, 1);
    }

    /// The sample interval is five seconds' worth of the machine's ticks.
    #[test]
    fn a_sample_lands_once_every_five_seconds_of_ticks() {
        let _lock = crate::sched::test_lock::take();
        let mut samples = 0;
        for _ in 0..2500 {
            if sample_tick(250) {
                samples += 1;
            }
        }
        assert_eq!(samples, 2);
    }
}
