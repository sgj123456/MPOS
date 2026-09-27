#![no_main]

//! The civil calendar, in both directions.
//!
//! `date` is what `clock_gettime(CLOCK_REALTIME)` becomes once something asks
//! for a broken-down time, and what a `settimeofday` from userland turns back
//! into a day count. It is pure Hinnant arithmetic over `i64`, which means the
//! failure mode is not a panic but a date that is quietly a year off - and the
//! only way to say it is not is to run the two halves against each other over
//! the whole range, including the century and 400-year years where the leap
//! rule changes its mind.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::date;

#[derive(Arbitrary, Debug)]
struct Input {
    year: i64,
    /// Deliberately unfiltered: month 0 and day 40 are what a bogus
    /// `struct tm` from userland actually contains.
    month: u32,
    day: u32,
    days: i64,
}

fuzz_target!(|input: Input| {
    // Backwards first, because it is defined for every day count: the date it
    // names must be a real date, and counting it forward again must land on
    // the same day.
    let (year, month, day) = date::civil_from_epoch(input.days);
    assert!(
        (1..=12).contains(&month),
        "{} days from the epoch is month {month}",
        input.days
    );
    assert!(
        day >= 1 && day <= days_in_month(year, month),
        "{} days from the epoch is {year}-{month:02}-{day:02}, which is not a day",
        input.days
    );
    assert_eq!(
        date::days_from_civil(year, month, day),
        input.days.clamp(date::DAYS_MIN, date::DAYS_MAX),
        "{year}-{month:02}-{day:02} does not count back to {}",
        input.days
    );

    // Forwards, where the input is a date a real `struct tm` could carry.
    // The year is taken modulo the domain `date` documents - outside
    // `YEAR_MIN..=YEAR_MAX` the function clamps rather than computing a date,
    // and a round trip through a clamp is not a round trip.
    let year = input.year.rem_euclid(date::YEAR_MAX - date::YEAR_MIN) + date::YEAR_MIN;
    let month = input.month % 12 + 1;
    let day = input.day % 28 + 1;
    let counted = date::days_from_civil(year, month, day);
    let (back_year, back_month, back_day) = date::civil_from_epoch(counted);
    assert_eq!(
        (back_year, back_month, back_day),
        (year, month, day),
        "{:?} did not survive a round trip through the epoch",
        (year, month, day)
    );

    // The epoch itself, and the two leap rules, are the fixed points the whole
    // module is calibrated on.
    assert_eq!(date::days_from_civil(1970, 1, 1), 0);
    assert_eq!(date::civil_from_epoch(0), (1970, 1, 1));
    assert_eq!(
        date::days_from_civil(2000, 3, 1) - date::days_from_civil(2000, 2, 28),
        2
    );
    assert_eq!(
        date::days_from_civil(1900, 3, 1) - date::days_from_civil(1900, 2, 28),
        1
    );
});

/// How many days a month has, including the leap rule the arithmetic is being
/// tested against - written independently of `date`, so it can disagree.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 != 0 || (year % 100 == 0 && year % 400 != 0) {
                28
            } else {
                29
            }
        }
        _ => 0,
    }
}
