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
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Busy-wait for roughly `ms` milliseconds. Only for boot-time tests and
/// driver bring-up; the real sleep path arrives with the scheduler in M4.
pub fn busy_wait_ms(ms: u64) {
    let deadline = ticks() + ms * (TICK_HZ as u64) / 1000;
    while ticks() < deadline {
        core::hint::spin_loop();
    }
}
