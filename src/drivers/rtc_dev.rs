//! `/dev/rtc0` - the CMOS clock as a device node, which is the door
//! `hwclock(8)` reaches through.
//!
//! This module is `rtc_dev` and not `rtc` because the clock itself is
//! [`crate::arch::x86_64::rtc`]: the code that reads and writes the CMOS,
//! which the kernel's own `clock_gettime` anchor comes from. What lives here
//! is the *node* in front of it - the file, its char-device number, and the
//! stream face - and two modules called `rtc` in one tree, one a driver and
//! one a device, is how a reader learns to distrust the names.
//!
//! The kernel already keeps its wall clock *in* this hardware (the boot
//! anchor comes from the CMOS; see `arch::x86_64::rtc`), but until now no
//! name led there: OpenRC's `sysclock` service ran `hwclock --hctosys`, got
//! ENOENT on every fallback device name, and the boot log recorded "Failed
//! to set the system clock" on every boot.
//!
//! What the node reports is the kernel's own wall clock - boot epoch plus
//! ticks - not a fresh CMOS read. One clock, one answer: `date`,
//! `clock_gettime` and `hwclock -r` cannot disagree, which is the property
//! that matters to the TLS validators and timestamp writers this exists for.
//! The ioctls that speak the fields (`RTC_RD_TIME`, `RTC_SET_TIME`) are
//! answered in `syscall::linux::fs`, keyed on the device number this node
//! carries; the alarm/interrupt half of the RTC ioctl family is not
//! implemented and says so as ENOTTY, which is what Linux answers for
//! requests its hardware does not do too.

use crate::vfs::devfs::CharDevice;
use crate::vfs::{VfsError, VfsResult};

/// The stream face of the clock.
///
/// A read on the real device yields the count of alarm/UIE interrupts since
/// the last read, and blocks while interrupts are enabled; neither is wired
/// up here, so a read finds no interrupt to report - end of input, and a
/// caller that enabled nothing (hwclock, everything at boot) never asks.
pub struct RtcDev;

impl CharDevice for RtcDev {
    fn read(&self, _buf: &mut [u8]) -> crate::vfs::VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> usize {
        // Writes set the alarm registers on Linux. There is no alarm to set,
        // and refusing would turn a probe into a boot-log warning; accept
        // the bytes as the entropy device accepts its writes.
        buf.len()
    }
}

/// The kernel's current wall-clock seconds: exactly what
/// `clock_gettime(CLOCK_REALTIME)` answers with, from the same two numbers.
pub fn now_epoch() -> u64 {
    crate::hal::boot_epoch() + crate::hal::since_boot_nanos() / 1_000_000_000
}

/// Break a Unix second into the nine `int`s of `struct rtc_time`, in its
/// conventions: `tm_mon` is 0-based, `tm_year` is years since 1900, `tm_wday`
/// counts from Sunday (1970-01-01 was a Thursday - day 4), and this kernel
/// has no daylight saving to flag.
pub fn time_fields(epoch: u64) -> [i32; 9] {
    let secs_of_day = (epoch % 86_400) as i32;
    let days = (epoch / 86_400) as i64;
    let (year, month, day) = crate::date::civil_from_epoch(days);
    let jan1 = crate::date::days_from_civil(year, 1, 1);
    [
        secs_of_day % 60,
        secs_of_day / 60 % 60,
        secs_of_day / 3600,
        day as i32,
        month as i32 - 1,
        (year - 1900) as i32,
        ((days + 4) % 7) as i32,
        (days - jan1) as i32,
        0,
    ]
}

/// The inverse: `struct rtc_time` as userland wrote it back, into a Unix
/// second. `false` for a field set no calendar carries (month out of range,
/// a year before 1970) - the caller answers EINVAL to those, and leaves the
/// clock alone.
pub fn fields_to_epoch(f: &[i32; 9]) -> VfsResult<u64> {
    let [sec, min, hour, mday, mon, year, ..] = *f;
    if !(0..60).contains(&sec) || !(0..60).contains(&min) || !(0..24).contains(&hour) {
        return Err(VfsError::Invalid);
    }
    if !(1..=31).contains(&mday) || !(0..12).contains(&mon) || year < 70 {
        return Err(VfsError::Invalid);
    }
    let days = crate::date::days_from_civil(year as i64 + 1900, (mon + 1) as u32, mday as u32);
    if days < 0 {
        return Err(VfsError::Invalid);
    }
    Ok(days as u64 * 86_400 + (hour as u64 * 3600 + min as u64 * 60 + sec as u64))
}
