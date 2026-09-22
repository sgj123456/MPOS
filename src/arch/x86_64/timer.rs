//! 8253/8254 PIT channel 0 as the scheduler clock.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

const PIT_CHANNEL0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_BASE_HZ: u32 = 1_193_182;

/// Channel 0, lo/hi byte access, mode 2 (rate generator).
const CMD_CHANNEL0_MODE2: u8 = 0x34;

pub const TICK_HZ: u32 = 100;

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
pub fn since_boot_nanos() -> u64 {
    let tick_ns = 1_000_000_000 / TICK_HZ as u64;
    let ticks = TICKS.load(Ordering::Relaxed);
    let cpt = CYCLES_PER_TICK.load(Ordering::Relaxed);
    let fraction = if cpt == 0 {
        0
    } else {
        let since = rdtsc().wrapping_sub(LAST_TICK_TSC.load(Ordering::Relaxed));
        (since % cpt) * tick_ns / cpt
    };
    ticks * tick_ns + fraction
}

/// Busy-wait for roughly `ms` milliseconds. Only for boot-time tests and
/// driver bring-up; the real sleep path arrives with the scheduler in M4.
pub fn busy_wait_ms(ms: u64) {
    let deadline = ticks() + ms * (TICK_HZ as u64) / 1000;
    while ticks() < deadline {
        core::hint::spin_loop();
    }
}
