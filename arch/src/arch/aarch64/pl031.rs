//! The PL031 real-time clock, as QEMU's `virt` machine wires it.
//!
//! This is `crate::hal::WallClock`'s hardware end on the second machine - the
//! missing trait that, with stage 6c's paging methods, is all that stood between
//! `hal` and compiling here (`NOTES.md` §9.1). The generic contract is "the Unix
//! epoch the boot anchored wall time at", the offset `clock_gettime(CLOCK_REALTIME)`
//! adds to uptime.
//!
//! The clock **reads and writes**, and both halves were measured on the machine
//! rather than taken from the register map. At boot `RTC_DR` reads the host's real
//! UTC epoch with `CR=0x1`: QEMU's model derives `DR` from the host clock plus an
//! offset, and shadows the host clock the way the x86 CMOS module it mirrors does.
//! So a cold boot gives a genuine wall time to anchor to, and `boot_epoch` reads it
//! once at `init` the way x86 reads the CMOS once at `init`. Writing `RTC_LR` moves
//! that offset, so `DR` then reads the value written - measured: loading
//! `0x6ac360c7` left `DR` reading `0x6ac360c7`.
//!
//! That second half is a correction. This module used to answer `write_epoch` with
//! `false`, on the recorded conclusion that the counter was read-only ("a naive
//! write left both `RTC_DR` and `RTC_LR` reading the lock key"). The conclusion came
//! from writing the wrong register: `RTC_DR` is the counter's *read* door, and the
//! load door is `RTC_LR` at `0x08`. What made it worth re-measuring is that the
//! refusal was visible in a real boot: with no writable clock this machine could not
//! back `/dev/rtc0`'s `RTC_SET_TIME`, which is a promise `UAPI.md` already records
//! as served, and OpenRC's `hwclock` service runs `hwclock --systohc` on the way
//! down. `write_epoch` now loads the counter and reads it back, so a write the
//! device dropped is still a `false` rather than a success (`AGENTS.md` §4).
//!
//! `set_boot_epoch` is a different thing and stays one: it moves the *in-memory*
//! anchor `clock_gettime` adds uptime to, the way `settimeofday` does on both
//! machines, and it is what writing the hardware clock does not do on either (Linux
//! writing an RTC does not move the system clock).
//!
//! The clock is reached through the kernel half's device window like every other
//! peripheral here - `0x0901_0000` is where the tree places `arm,pl031`, one region
//! above the PL011 - and the boot check pins that reading it through the window
//! yields a live clock, not zeroes or the wrong alias.

use super::paging;
use core::sync::atomic::{AtomicU64, Ordering};

/// The physical address of the PL031 on QEMU's `virt` machine: the tree's
/// `pl031@9010000` node, one 64 KiB region above the PL011.
pub const PHYS: usize = 0x0901_0000;

/// The clock as this kernel reaches it: the device window, at this device's offset
/// into it - the same spelling [`super::pl011::BASE`] uses, so a window that moves
/// takes this with it.
pub const BASE: usize = paging::DEVICE_VIRT + (PHYS - paging::DEVICE_PHYS);

/// Data register: the seconds the counter has run. Derived from the host clock plus
/// the load offset, so reading it twice a second apart differs by one; it is the
/// epoch `init` anchors wall time to, and the register `write_epoch` verifies against.
const DR: usize = 0x00;

/// Load register: writing it moves the offset `DR` is derived from, which is the
/// whole of what setting this clock is. A write here is what `RTC_SET_TIME` reaches
/// the hardware through.
const LR: usize = 0x08;

fn read(offset: usize) -> u32 {
    // SAFETY: the offset is a register of the PL031 at `BASE`, and the device
    // window is mapped as soon as the machine starts, so the page is reachable
    // without this module doing any mapping of its own.
    unsafe { core::ptr::read_volatile((BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as `read` - the same device, the same page, a register this module
    // owns the meaning of.
    unsafe { core::ptr::write_volatile((BASE + offset) as *mut u32, value) }
}

/// The wall epoch this boot anchored wall time at, in seconds. `boot_epoch`'s answer
/// and `set_boot_epoch`'s target, exactly as the x86 CMOS module keeps the same
/// anchor: `wall time = BOOT_EPOCH + uptime`. Seeded from the counter at `init`.
static BOOT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The counter's current value, as the seconds it names.
pub fn read_epoch() -> u64 {
    read(DR) as u64
}

/// Snapshot the clock into `BOOT_EPOCH`. Called once during arch init, before
/// anything else can race it - the same one-read-at-boot the x86 module does.
pub fn init() {
    BOOT_EPOCH.store(read_epoch(), Ordering::Relaxed);
}

/// The Unix epoch this boot anchored wall time at. Read from the PL031 at `init`;
/// on this machine that is the host's real UTC time, so there is no "epoch zero"
/// fallback to degrade to unless the counter is read before `init`.
pub fn boot_epoch() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed)
}

/// Move the boot anchor, the way `settimeofday` and `clock_settime` do: the tick
/// counter keeps running and the epoch it is measured from moves underneath it.
pub fn set_boot_epoch(epoch: u64) {
    BOOT_EPOCH.store(epoch, Ordering::Relaxed);
}

/// Load `epoch` into the counter, the way `hwclock -w` and `RTC_SET_TIME` do.
///
/// `false` for an epoch the counter cannot spell, and nothing is written in that
/// case: `RTC_DR` is thirty-two bits of seconds, so the calendar it can hold runs
/// out in 2106 - the same shape as the x86 CMOS module refusing a year its registers
/// would decode wrong. The read-back is checked rather than assumed, because the
/// trait's contract is that a dropped write is a refusal and not a success.
pub fn write_epoch(epoch: u64) -> bool {
    if epoch > u32::MAX as u64 {
        return false;
    }
    write(LR, epoch as u32);
    // The counter kept running across the write - a load sets what `DR` reads
    // *now* - so a second rolling over between the store and the read is a landed
    // write and not a dropped one. One second of slack is enough and cannot mask
    // a failure: a write the device ignored leaves the epoch that was there
    // before (this decade's) where the caller asked for whatever it asked for,
    // and no two of those are ever within a second of each other.
    let got = read_epoch();
    got == epoch || got == epoch + 1
}
