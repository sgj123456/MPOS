//! The CMOS RTC as the source of wall-clock time.
//!
//! The PIT counts uptime, and uptime alone - a `clock_gettime` built on it
//! answers as if the epoch were the moment of boot. Anything in userland that
//! checks a date against the calendar (TLS certificate validity above all)
//! fails on such a clock: every certificate's `notBefore` is in the future.
//!
//! The real-time clock in the CMOS is where a PC keeps the calendar. QEMU
//! maintains it from the host's clock, so reading it at boot gives the kernel
//! a genuine wall time to anchor the tick counter to: wall time becomes
//! `boot epoch + uptime`, monotonic time stays uptime.
//!
//! All of this runs once at boot, before anything else can race it, so no
//! locking is needed beyond the atomics.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

/// Register indexes within the CMOS. Bit 7 of the address port selects NMI;
/// writes here keep it set (disabled) - NMI handling is not wired up, and an
/// unexpected NMI would fault anyway.
const RTC_SEC: u8 = 0x00;
const RTC_MIN: u8 = 0x02;
const RTC_HOUR: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_STATUS_A: u8 = 0x0a;
const RTC_STATUS_B: u8 = 0x0b;
/// The century register has no architecture-mandated index; 0x32 is what the
/// ACPI spec names and what QEMU implements. A read of zero (a board that
/// keeps no century there) falls back to the 2000s below.
const RTC_CENTURY: u8 = 0x32;

/// Status register A, update-in-progress flag.
const STATUS_A_UIP: u8 = 1 << 7;
/// Status register B format bits.
const STATUS_B_24H: u8 = 1 << 1;
const STATUS_B_BINARY: u8 = 1 << 2;

/// Wall-clock seconds at boot, as a Unix timestamp.
static BOOT_EPOCH: AtomicU64 = AtomicU64::new(0);

fn cmos_read(index: u8) -> u8 {
    unsafe {
        let mut addr: Port<u8> = Port::new(CMOS_ADDR);
        let mut data: Port<u8> = Port::new(CMOS_DATA);
        addr.write(0x80 | index);
        data.read()
    }
}

/// Decode one RTC field according to the formats status register B names.
/// A board in BCD mode stores every field as two decimal nibbles.
fn decode(raw: u8, status_b: u8) -> u8 {
    if status_b & STATUS_B_BINARY != 0 {
        raw
    } else {
        (raw & 0x0f) + (raw >> 4) * 10
    }
}

/// Days from 1970-01-01, and the date a day-count names: the pure halves of
/// the calendar live in [`crate::date`], where a host build can test them;
/// this file is only the wires to the CMOS.
use crate::date::civil_from_epoch;
use crate::date::days_from_civil;

/// Read the clock once, converting to Unix seconds.
///
/// The RTC has no atomic multi-field read: while it updates its copy of the
/// time (up to once a second, flagged by status A's UIP bit) a read straddling
/// that update can mix two values - 59 seconds after a minute rollover, say.
/// The standard guard is UIP polling plus a read-twice-and-compare loop, so a
/// torn read is detected and retried rather than shipped.
fn read_epoch() -> u64 {
    // Wait out an update already in flight. UIP asserts for ~2 ms out of
    // every second; a bounded spin keeps a wedged RTC from hanging the boot.
    for _ in 0..1_000_000 {
        if cmos_read(RTC_STATUS_A) & STATUS_A_UIP == 0 {
            break;
        }
        core::hint::spin_loop();
    }

    let status_b = cmos_read(RTC_STATUS_B);

    // Two full reads, compared field by field; UIP is re-checked between
    // them. On agreement the values are clean. About ten attempts bounds the
    // loop even on a machine updating every time we look, and QEMU's clock
    // agrees on the first try.
    for _ in 0..10 {
        let first = read_fields(status_b);
        if cmos_read(RTC_STATUS_A) & STATUS_A_UIP != 0 {
            continue;
        }
        let second = read_fields(status_b);
        if first == second {
            return to_epoch(first, status_b);
        }
    }
    // Ten torn reads in a row is not a state a working clock reaches. Fall
    // back to the last read rather than nothing - a roughly-right clock beats
    // the epoch by any margin.
    let last = read_fields(status_b);
    to_epoch(last, status_b)
}

#[derive(PartialEq)]
struct RtcFields {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
}

fn read_fields(status_b: u8) -> RtcFields {
    RtcFields {
        sec: decode(cmos_read(RTC_SEC), status_b),
        min: decode(cmos_read(RTC_MIN), status_b),
        hour: decode(cmos_read(RTC_HOUR), status_b),
        day: decode(cmos_read(RTC_DAY), status_b),
        month: decode(cmos_read(RTC_MONTH), status_b),
        year: decode(cmos_read(RTC_YEAR), status_b),
        century: decode(cmos_read(RTC_CENTURY), status_b),
    }
}

/// Convert one field set to Unix seconds.
///
/// The RTC keeps local time as the machine sees it, which for QEMU's default
/// (`-rtc base=utc`) is UTC - the only convention a kernel can assume without
/// a timezone database. Hours come in two encodings (12-hour reads carry a PM
/// bit at 0x80); normalise before the arithmetic.
fn to_epoch(f: RtcFields, status_b: u8) -> u64 {
    let mut hour = f.hour;
    if status_b & STATUS_B_24H == 0 {
        // 12-hour mode: 0x80 is PM, and 12 PM is noon, not zero.
        let pm = hour & 0x80 != 0;
        hour &= 0x7f;
        if hour == 12 {
            hour = if pm { 12 } else { 0 };
        } else if pm {
            hour += 12;
        }
    }

    let century: i64 = if f.century >= 19 {
        f.century as i64 * 100
    } else {
        2000
    };
    let year = century + f.year as i64;
    let secs_of_day = hour as u64 * 3600 + f.min as u64 * 60 + f.sec as u64;
    (days_from_civil(year, f.month as u32, f.day as u32) as u64) * 86_400 + secs_of_day
}

/// Snapshot the clock into `BOOT_EPOCH`. Called once during arch init, before
/// interrupts are enabled - no driver can move the time under us.
pub fn init() {
    let epoch = read_epoch();
    BOOT_EPOCH.store(epoch, Ordering::Relaxed);
}

/// Unix seconds at boot, or 0 if the RTC has not been read. The 0 case is
/// only reachable before `init` - the wall-clock conversions add this to the
/// tick count, and a missing anchor degrades to the old uptime-as-epoch
/// behaviour rather than to a negative time.
pub fn boot_epoch() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed)
}

/// Move the boot anchor, the way `settimeofday(2)` and `clock_settime(2)`
/// set the wall clock: the tick counter keeps running, and the epoch it is
/// measured from moves underneath it. The one discontinuity this makes is
/// exactly the one those calls promise - the wall clock jumps - and the
/// timers that asked to hear about it are told, by the caller that reaches
/// for the anchor.
pub fn set_boot_epoch(epoch: u64) {
    BOOT_EPOCH.store(epoch, Ordering::Relaxed);
}

fn cmos_write(index: u8, value: u8) {
    unsafe {
        let mut addr: Port<u8> = Port::new(CMOS_ADDR);
        let mut data: Port<u8> = Port::new(CMOS_DATA);
        addr.write(0x80 | index);
        data.write(value);
    }
}

/// Encode one field into the format status register B names - `decode`'s
/// exact inverse, in both directions and under every quirk of this board's
/// status byte. Writing through the inverse of the reader is what makes the
/// pair self-consistent: whatever the format bits end up meaning, a write
/// here reads back there.
fn encode(value: u8, status_b: u8) -> u8 {
    if status_b & STATUS_B_BINARY != 0 {
        value
    } else {
        ((value / 10) << 4) | (value % 10)
    }
}

/// Store a wall-clock time in the CMOS, the way `RTC_SET_TIME` does it: the
/// fields go out in the format the reader expects, and the year is split
/// across the century register the way `to_epoch` reassembles it.
///
/// Status register B is deliberately left untouched. The conventional SET bit
/// would freeze the clock's own update while the fields change, but writing
/// that bit wrong on a board with a shifted bit layout stops the clock from
/// ever updating again - and a field write that races a second's update is
/// worth less than the next second's update, which on the machines this
/// kernel boots (QEMU, whose RTC shadows the host clock) overwrites these
/// registers within a tick anyway. What the write buys is the honest
/// `hwclock -w` round trip - read the clock back and it says the time you
/// set - which is what userland actually checks.
///
/// `false` when the epoch is outside the range the registers can spell
/// (years 1900..2500 - a century byte past 25 would decode wrong); nothing
/// is written in that case.
pub fn write_epoch(epoch: u64) -> bool {
    let secs_of_day = epoch % 86_400;
    let days = (epoch / 86_400) as i64;
    let (year, month, day) = civil_from_epoch(days);
    if !(1900..2500).contains(&year) || month == 0 || month > 12 || day == 0 || day > 31 {
        return false;
    }

    let status_b = cmos_read(RTC_STATUS_B);
    let mut hour = (secs_of_day / 3600) as u8;
    if status_b & STATUS_B_24H == 0 {
        // 12-hour mode, exactly as `to_epoch` normalises it on the way in:
        // the PM bit at 0x80, twelve o'clock spelled as 12 rather than 0.
        let pm = hour >= 12;
        hour %= 12;
        if hour == 0 {
            hour = 12;
        }
        if pm {
            hour |= 0x80;
        }
    }

    // Wait out an in-flight update, bounded like the reader does, so the
    // fields do not land across the clock's own write of the next second.
    for _ in 0..1_000_000 {
        if cmos_read(RTC_STATUS_A) & STATUS_A_UIP == 0 {
            break;
        }
        core::hint::spin_loop();
    }

    cmos_write(RTC_SEC, encode((secs_of_day % 60) as u8, status_b));
    cmos_write(RTC_MIN, encode((secs_of_day / 60 % 60) as u8, status_b));
    cmos_write(RTC_HOUR, encode(hour, status_b));
    cmos_write(RTC_DAY, encode(day as u8, status_b));
    cmos_write(RTC_MONTH, encode(month as u8, status_b));
    cmos_write(RTC_YEAR, encode((year % 100) as u8, status_b));
    cmos_write(RTC_CENTURY, encode((year / 100) as u8, status_b));
    true
}
