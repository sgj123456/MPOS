//! The PL031 real-time clock, as QEMU's `virt` machine wires it.
//!
//! This is `crate::hal::WallClock`'s hardware end on the second machine - the
//! missing trait that, with stage 6c's paging methods, is all that stood between
//! `hal` and compiling here (`PLAN.md` §9.1). The generic contract is "the Unix
//! epoch the boot anchored wall time at", the offset `clock_gettime(CLOCK_REALTIME)`
//! adds to uptime.
//!
//! Measured on this machine, and it changed the design: at boot `RTC_DR` reads
//! `0x6ac0e25a` - the host's real UTC epoch, `CR=0x1` (QEMU's model starts the
//! counter and shadows the host clock, exactly like the x86 CMOS module this
//! mirrors). So a cold boot here *does* give a genuine wall time to anchor to, and
//! `boot_epoch` reads it once at init the way x86 reads the CMOS once at init. What
//! this machine does **not** have is a *writable* persistent register: `RTC_DR` is
//! derived from the host clock and a programmatic write does not stick (a naive
//! write left both `RTC_DR` and `RTC_LR` reading the lock key - the register map is
//! not the model). So `write_epoch` answers `false`: `hal::WallClock`'s own doc says
//! "`false` means there is no persistent clock to write", and that is this machine's
//! truthful answer rather than a write that looks like it worked. `set_boot_epoch`
//! still moves the in-memory anchor, the way `settimeofday` does on both machines.
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

/// Data register: the seconds the counter has run. Read-only on QEMU's model, which
/// derives it from the host clock; it is the epoch `init` anchors wall time to.
const DR: usize = 0x00;

fn read(offset: usize) -> u32 {
    // SAFETY: the offset is a register of the PL031 at `BASE`, and the device
    // window is mapped as soon as the machine starts, so the page is reachable
    // without this module doing any mapping of its own.
    unsafe { core::ptr::read_volatile((BASE + offset) as *const u32) }
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

/// There is no writable persistent clock to write.
///
/// QEMU's PL031 `RTC_DR` is a host-shadowed counter, not a stored epoch, so unlike
/// the x86 CMOS (which `hwclock -w` writes) this machine has no register to persist
/// a set time into. `false` is the honest answer the trait names for exactly this
/// case; a write that appears to succeed and is ignored by the device would be the
/// worst kind of lie (`AGENTS.md` §4).
pub fn write_epoch(_epoch: u64) -> bool {
    false
}
