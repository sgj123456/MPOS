//! 8253/8254 PIT channel 0 as the scheduler clock.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

const PIT_CHANNEL0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_BASE_HZ: u32 = 1_193_182;

/// Channel 0, lo/hi byte access, mode 2 (rate generator).
const CMD_CHANNEL0_MODE2: u8 = 0x34;

/// PIT ticks per second - the scheduling and poll-granularity clock.
///
/// Time *reading* no longer depends on this (the TSC fills sub-tick detail in
/// `since_boot_nanos`), so it can be raised purely to cut scheduling latency:
/// a wake of a same-or-higher-priority task is honored at the next tick, so the
/// tick period is the worst-case hand-off wait. 250 Hz (Linux's usual `HZ`)
/// makes that 4 ms instead of the old 10 ms, at 2.5× the timer-interrupt load.
pub const TICK_HZ: u32 = 250;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// The time-stamp counter as of the most recent tick, and the counter delta
/// that one tick took. Reading `rdtsc` here is what lets `clock_gettime` report
/// a fraction of the current 10 ms tick instead of snapping to its start.
///
/// `cycles_per_tick` is self-calibrating from the PIT: the gap between two
/// consecutive ticks *is* one tick's worth of counter increments, sampled with
/// no boot-time stall. It is zero until the second tick lands, and every reader
/// degrades to the old whole-tick answer until then.
static LAST_TICK_TSC: AtomicU64 = AtomicU64::new(0);
static CYCLES_PER_TICK: AtomicU64 = AtomicU64::new(0);

/// The x86_64 time-stamp counter: free-running, monotonic, at the CPU's TSC
/// rate. `lfence` serialises the read so it cannot slip past a later memory
/// access.
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "lfence",
            "rdtsc",
            lateout("eax") lo,
            lateout("edx") hi,
            options(nomem, nostack)
        );
    }
    ((hi as u64) << 32) | lo as u64
}

pub fn init() {
    let divisor = (PIT_BASE_HZ / TICK_HZ) as u16;
    unsafe {
        let mut cmd: Port<u8> = Port::new(PIT_CMD);
        let mut ch0: Port<u8> = Port::new(PIT_CHANNEL0);
        cmd.write(CMD_CHANNEL0_MODE2);
        ch0.write((divisor & 0xff) as u8);
        ch0.write((divisor >> 8) as u8);
    }
    super::pic::set_masked(0, false);
}

pub fn tick() {
    let now = rdtsc();
    let last = LAST_TICK_TSC.swap(now, Ordering::Relaxed);
    // A tick that follows another measured the length of one tick.
    if last != 0 {
        CYCLES_PER_TICK.store(now.wrapping_sub(last), Ordering::Relaxed);
    }
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Nanoseconds since boot, to the resolution the TSC gives - well under the
/// 10 ms tick the counter alone would allow. The whole ticks are exact; the
/// partway-into-the-current-tick fraction is the elapsed cycles since the last
/// tick, scaled by the calibrated cycles-per-tick. Returns whole ticks only
/// (fraction zero) until two ticks have been seen.
///
/// The three words and the counter sample are read with interrupts off, and
/// that is not an optimisation but the fix for a real backwards step. `tick`
/// writes `LAST_TICK_TSC`, then `CYCLES_PER_TICK`, then `TICKS`, and a reader
/// that loaded them one at a time could be interrupted between any two - taking
/// a `last` from one tick beside a `ticks` from the next (or a counter sample
/// from behind `last`). The subtraction then runs against a sample that is
/// *before* `last`, wrapping to a huge number whose `% cpt` lands anywhere in a
/// tick, so the clock could read *earlier* than it had a microsecond before.
/// `t-clock-gettime`'s "two reads never go backwards" is what caught it, and it
/// flaked rather than failed because it needs the tick to land inside the read.
/// Interrupts off is this kernel's mutual exclusion - one core, one writer - so
/// the snapshot is one reading; the section's `cli`/`sti` are compiler barriers
/// besides, so no load escapes it.
pub fn since_boot_nanos() -> u64 {
    let tick_ns = 1_000_000_000 / TICK_HZ as u64;
    let (ticks, last, cpt, now) = critical_section::with(|_| {
        (
            TICKS.load(Ordering::Relaxed),
            LAST_TICK_TSC.load(Ordering::Relaxed),
            CYCLES_PER_TICK.load(Ordering::Relaxed),
            rdtsc(),
        )
    });
    // Before calibration there is no fraction to add and the divisor below
    // would be zero: the whole ticks are all the clock has.
    if cpt == 0 {
        return ticks * tick_ns;
    }
    // The tick that would move `last` cannot have fired inside the section, so
    // `since` is the part of tick `ticks` that has run. The clamp is for a
    // `cpt` that under-measures the tick it scales: holding the fraction at the
    // top keeps the reading still there, where resetting it to zero would let
    // it step back by up to a tick.
    let since = now.wrapping_sub(last);
    let frac = if since >= cpt { cpt - 1 } else { since };
    ticks * tick_ns + frac * tick_ns / cpt
}

/// Busy-wait for roughly `ms` milliseconds. Only for boot-time tests and
/// driver bring-up; the real sleep path arrives with the scheduler in M4.
pub fn busy_wait_ms(ms: u64) {
    let deadline = ticks() + ms * (TICK_HZ as u64) / 1000;
    while ticks() < deadline {
        core::hint::spin_loop();
    }
}
