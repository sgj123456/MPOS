//! The ARM generic timer, as this machine's clock.
//!
//! The same role the 8254 PIT plays on x86_64 and the same `TICK_HZ`, so the
//! two machines' notion of a tick is the same number rather than merely both
//! present - which is what lets the scheduler's policy code be shared when
//! stage 3 un-gates it.
//!
//! What differs is where the time comes from. The PIT is a divisor counted down
//! by a fixed input clock, and x86_64 has to calibrate the TSC against it to
//! say anything finer than a tick. Here `CNTFRQ_EL0` states the counter's rate
//! and `CNTPCT_EL0` *is* a free-running count of it, so a nanosecond answer is
//! a division rather than an estimate - `since_boot_nanos` is exact to the
//! counter, and there is no calibration state to be zero until the second tick.
//!
//! The interrupt is the EL1 *physical* timer (`CNTP_*`), whose PPI the machine
//! wires to INTID 30 ([`super::gic::TIMER_INTID`]). Reading and writing those
//! registers requires EL1 and no EL2 in the way, which is why
//! [`super::init_interrupts`] checks the exception level first rather than
//! letting a timer that never fires look like a broken vector table.

use core::sync::atomic::{AtomicU64, Ordering};

/// Ticks per second: the scheduler's and the poll layer's granularity, the same
/// 250 Hz the x86_64 PIT is programmed for (4 ms per tick).
pub const TICK_HZ: u32 = 250;

static TICKS: AtomicU64 = AtomicU64::new(0);
/// `CNTFRQ_EL0`: the counter's rate in Hz, as the machine states it.
static FREQ: AtomicU64 = AtomicU64::new(0);
/// How many counts one tick lasts, from [`FREQ`].
static PERIOD: AtomicU64 = AtomicU64::new(0);
/// `CNTPCT_EL0` at bring-up, so elapsed counts can be turned into elapsed time.
static BOOT_COUNT: AtomicU64 = AtomicU64::new(0);
/// The next absolute deadline in counter counts, which is what
/// `CNTP_CVAL_EL0` holds.
static NEXT: AtomicU64 = AtomicU64::new(0);

fn read_cntfrq() -> u64 {
    let value: u64;
    // SAFETY: `CNTFRQ_EL0` and `CNTPCT_EL0` are read-only system registers,
    // readable at EL1 with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

fn read_cntpct() -> u64 {
    let value: u64;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

/// Set the *absolute* deadline the counter has to reach.
///
/// Absolute rather than the relative `CNTP_TVAL_EL0` form, and that is the
/// difference between a clock and a clock that slowly stops agreeing with
/// itself. A relative re-arm in the interrupt handler schedules the next tick
/// from the moment the handler *ran*, so every tick's interrupt latency is added
/// to the period and the error accumulates: measured, the relative form gave 24
/// ticks in the 100 ms this machine's check waits for, against the 25 the
/// counter says should fit. With the deadline computed from the previous
/// deadline, the handler being late costs a late tick and nothing more, and the
/// count stays at 250 Hz for as long as the machine runs.
fn set_deadline(counts: u64) {
    // SAFETY: `CNTP_CVAL_EL0` is the EL1 physical timer's compare value; the
    // write is the documented way to arm it with an absolute deadline.
    unsafe {
        core::arch::asm!("msr cntp_cval_el0, {}", in(reg) counts, options(nomem, nostack));
    }
}

/// Enable the EL1 physical timer and unmask its interrupt.
fn enable() {
    // SAFETY: `CNTP_CTL_EL0` bit 0 is ENABLE, bit 1 is IMASK.
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack));
    }
}

/// The counter's rate in Hz, as the machine reports it. Zero before [`init`].
pub fn frequency() -> u64 {
    FREQ.load(Ordering::Relaxed)
}

/// Counts per tick, as [`init`] computed them. Zero before [`init`].
pub fn period() -> u64 {
    PERIOD.load(Ordering::Relaxed)
}

/// Start the clock: read the rate, arm the first period, unmask the interrupt.
pub fn init() {
    let freq = read_cntfrq();
    assert!(freq > 0, "CNTFRQ_EL0 is zero: there is no timer to read");
    let period = freq / TICK_HZ as u64;
    assert!(period > 0, "the counter runs slower than the tick rate");
    FREQ.store(freq, Ordering::Relaxed);
    PERIOD.store(period, Ordering::Relaxed);
    let now = read_cntpct();
    BOOT_COUNT.store(now, Ordering::Relaxed);
    NEXT.store(now.wrapping_add(period), Ordering::Relaxed);
    set_deadline(now.wrapping_add(period));
    enable();
}

/// Re-arm the timer to fire one period from now, leaving the tick count alone.
///
/// A check that needs a *guaranteed pending interrupt* without disturbing the
/// count already taken - the idle path, which must be woken by something. The
/// compare is a level source, so moving the deadline into the near future asserts
/// the line once more exactly one tick later.
pub fn arm_next_period() {
    let now = read_cntpct();
    let deadline = now.wrapping_add(PERIOD.load(Ordering::Relaxed));
    NEXT.store(deadline, Ordering::Relaxed);
    set_deadline(deadline);
    enable();
}

/// The counter's raw value, for callers that need the machine's own units.
pub fn counter() -> u64 {
    read_cntpct()
}

/// `CNTP_CTL_EL0`: bit 0 enabled, bit 1 masked, bit 2 the condition reached.
///
/// Read by a failed bring-up check: bit 2 set means the timer *did* fire and
/// the interrupt is missing somewhere above it, and clear means the timer
/// itself is the thing that is not working.
pub fn control() -> u64 {
    let value: u64;
    // SAFETY: `CNTP_CTL_EL0` is readable at EL1 with no side effects.
    unsafe {
        core::arch::asm!("mrs {}, cntp_ctl_el0", out(reg) value, options(nomem, nostack));
    }
    value
}

/// One clock interrupt: count it and move the deadline on by one period.
///
/// Called from the IRQ vector, so it takes no lock and allocates nothing - two
/// atomics and one system register are the whole of its work. A deadline already
/// in the past (because this ran late) fires again immediately, so the count
/// catches up rather than drifting.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    let next = NEXT
        .load(Ordering::Relaxed)
        .wrapping_add(PERIOD.load(Ordering::Relaxed));
    NEXT.store(next, Ordering::Relaxed);
    set_deadline(next);
}

/// Monotonic ticks since the clock started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Nanoseconds since [`init`], from the free-running counter.
///
/// Exact rather than estimated: the elapsed count divided by the rate the
/// machine states. The multiply is done in 128 bits because the count of a
/// multi-hour boot scaled by 10^9 does not fit in 64.
pub fn since_boot_nanos() -> u64 {
    let freq = FREQ.load(Ordering::Relaxed);
    if freq == 0 {
        // Before `init` the tick counter is all there is, and it says zero.
        return ticks().saturating_mul(1_000_000_000 / TICK_HZ as u64);
    }
    let elapsed = read_cntpct().wrapping_sub(BOOT_COUNT.load(Ordering::Relaxed));
    (u128::from(elapsed) * 1_000_000_000 / u128::from(freq)) as u64
}

/// Spin for approximately `ms` with interrupts as they are.
///
/// Derived from the counter rather than from ticks, so it is usable before the
/// clock is armed and does not depend on interrupts arriving at all - which is
/// what the bring-up check needs to distinguish "no ticks yet" from "no
/// interrupts".
pub fn busy_wait_ms(ms: u64) {
    let freq = FREQ.load(Ordering::Relaxed);
    if freq == 0 {
        return;
    }
    let deadline = read_cntpct().wrapping_add(freq / 1000 * ms);
    while read_cntpct() < deadline {
        core::hint::spin_loop();
    }
}
