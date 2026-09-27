//! Civil-calendar arithmetic, kept out of the architecture it serves for
//! the reason the `rand` and `logging` modules are kept out too: the sums
//! are pure, and a date rule with an off-by-one on one side of a leap year
//! is a date rule that shows up in `hwclock` on a compositor boot - not a
//! bug worth finding once at a time on the machine.
//!
//! `arch::x86_64::rtc` turns these into CMOS field writes and `struct
//! rtc_time` bytes; nothing here knows the hardware exists.

/// The year range the arithmetic below is exact for, and the range it clamps
/// to. `days_from_civil` multiplies the 400-year era by 146 097, so past about
/// 2.5e16 the answer no longer fits the `i64` it returns - and the widest date
/// anything here can be asked for is a CMOS century byte times a BCD year.
/// Outside this window there is no date to compute, and the one thing a
/// `settimeofday` cannot be allowed to do is panic the kernel over one.
pub const YEAR_MAX: i64 = 1_000_000_000;
pub const YEAR_MIN: i64 = -YEAR_MAX;

/// The first and last day count `civil_from_epoch` answers for: the ends of
/// the year range above, run through the forward direction, so the two
/// functions cannot drift apart on how far their domain reaches.
pub const DAYS_MIN: i64 = days_from_civil(YEAR_MIN, 1, 1);
pub const DAYS_MAX: i64 = days_from_civil(YEAR_MAX, 12, 31);

/// Days from 1970-01-01 to the given civil date, proleptic Gregorian.
///
/// Howard Hinnant's `days_from_civil`, the standard compact date arithmetic:
/// shift March to the start of the year so leap days land at the end, then
/// count 400-year eras where the leap pattern is exactly periodic.
///
/// The year is clamped into [`YEAR_MIN`]..=[`YEAR_MAX`] first; a date outside
/// that is not a date, and the sum has to say so by returning the nearest one
/// rather than by overflowing.
pub const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if y < YEAR_MIN {
        YEAR_MIN
    } else if y > YEAR_MAX {
        YEAR_MAX
    } else {
        y
    };
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64; // [0, 11], March = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The civil date a day-number from 1970-01-01 falls on: `days_from_civil`
/// run backwards, same era arithmetic, same Hinnant shape. `(year, month, day)`
/// with the month 1-based.
///
/// The day count is clamped into [`DAYS_MIN`]..=[`DAYS_MAX`] for the same
/// reason the year is clamped going forwards - within that range the two
/// directions are exact inverses of each other, and outside it there is
/// nothing to be exact about.
pub fn civil_from_epoch(days: i64) -> (i64, u32, u32) {
    let days = days.clamp(DAYS_MIN, DAYS_MAX);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `civil_from_epoch` is `days_from_civil` run backwards - and the pair
    /// is what `hwclock -r` reads and the CMOS write path stores, so one
    /// wrong leap-year day is a wrong date on every screen. Anchors first
    /// (the dates the arithmetic argues about), then a whole-days walk with
    /// a stride that lands in every month, so a century boundary and a leap
    /// February are round-tripped and not just spot-checked.
    #[test]
    fn civil_round_trips_the_anchors_and_the_calendar() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1900, 3, 1), -25_508); // 1900 was not a leap year
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(2100, 2, 28), 47_540); // 2100 will not be
        assert_eq!(civil_from_epoch(0), (1970, 1, 1));
        assert_eq!(civil_from_epoch(-1), (1969, 12, 31));

        for days in (-30_000..90_000).step_by(11) {
            let (y, m, d) = civil_from_epoch(days);
            assert_eq!(days_from_civil(y, m, d), days, "round trip of day {days}");
        }
    }

    /// The clamp is a total function, not a panic on the way to a refusal: a
    /// day count or a year off the end of the calendar has to come back as the
    /// nearest date that is on it. Checked at both ends, and one step in from
    /// each, because a boundary that only holds in one direction is the usual
    /// way an off-by-one survives.
    #[test]
    fn the_domain_clamps_rather_than_overflowing() {
        for days in [i64::MIN, i64::MAX, DAYS_MIN - 1, DAYS_MAX + 1] {
            let (y, m, d) = civil_from_epoch(days);
            assert!(
                (YEAR_MIN..=YEAR_MAX).contains(&y),
                "{days} days is year {y}"
            );
            assert_eq!(days_from_civil(y, m, d), days.clamp(DAYS_MIN, DAYS_MAX));
        }
        assert_eq!(civil_from_epoch(DAYS_MIN), (YEAR_MIN, 1, 1));
        assert_eq!(civil_from_epoch(DAYS_MAX), (YEAR_MAX, 12, 31));

        for y in [i64::MIN, i64::MAX, YEAR_MIN - 1, YEAR_MAX + 1] {
            let days = days_from_civil(y, 6, 15);
            assert!(
                (DAYS_MIN..=DAYS_MAX).contains(&days),
                "year {y} counts to {days}"
            );
        }
        // A number is only clamped when it is outside the calendar, so every
        // year a real clock could ever be asked to show is untouched.
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(
            civil_from_epoch(days_from_civil(999_999, 6, 15)),
            (999_999, 6, 15)
        );
    }
}
