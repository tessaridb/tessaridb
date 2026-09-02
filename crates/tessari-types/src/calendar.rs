//! The civil calendar an instant falls on.
//!
//! An instant is a second count. A year, a month and a day are a *reading* of
//! that count, and the reading is not obvious: it carries the leap-year rule,
//! the century exception, the four-century exception to the exception, and the
//! month lengths that depend on all three.
//!
//! # Why this is one implementation and not several
//!
//! The arithmetic was written once, for reading an instant out of text, and a
//! second copy was written for writing one back. That is already two, and the
//! language is about to ask for `time::year` and its six neighbours. Eight
//! extractors each deriving the date from a second count would be eight copies
//! of the same era arithmetic, and the way that fails is not a crash: two of
//! them disagree on one day in four hundred years, `time::day` answers `1` while
//! `time::month` answers the previous month, and nothing anywhere raises an
//! error.
//!
//! So the reading happens here, once, and everything that needs a date asks for
//! [`Civil`] — the text writer included. `text` reads and writes the *text*; how
//! a second count becomes a date is this module's single answer.

use crate::time::Datetime;

/// Seconds in the units the wall-clock fields carry.
pub(crate) const SECONDS_PER_MINUTE: i64 = 60;
pub(crate) const SECONDS_PER_HOUR: i64 = 3_600;
pub(crate) const SECONDS_PER_DAY: i64 = 86_400;

/// The date and wall-clock time an instant reads as, in UTC.
///
/// Fields rather than accessors: there is no invariant to protect that
/// [`Datetime::civil`] does not already establish, and a breakdown whose whole
/// purpose is to be read is worse for being read through six methods.
///
/// UTC, because a [`Datetime`] has no zone. A zone is a rendering choice made
/// where the value is displayed, and this type renders the moment that was
/// stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Civil {
    /// The proleptic Gregorian year; negative before year 1.
    pub year: i64,
    /// The month, 1 through 12.
    pub month: i64,
    /// The day of the month, 1 through 31.
    pub day: i64,
    /// The hour, 0 through 23.
    pub hour: i64,
    /// The minute, 0 through 59.
    pub minute: i64,
    /// The second, 0 through 59. Never 60: this store has no leap second, and
    /// its reader refuses one rather than folding it onto `:59`.
    pub second: i64,
}

impl Datetime {
    /// The date and time this instant falls on, in UTC.
    ///
    /// The sub-second remainder is not here. It is on the instant already
    /// ([`Datetime::nanos`]), and putting it in the breakdown too would give one
    /// value two homes.
    #[must_use]
    pub fn civil(self) -> Civil {
        let seconds = self.seconds();
        // Euclidean rather than truncating division throughout, which is what
        // makes an instant before the epoch read correctly: `-1` seconds is the
        // last second of 1969, not the first of a negative day.
        let days = seconds.div_euclid(SECONDS_PER_DAY);
        let within = seconds.rem_euclid(SECONDS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Civil {
            year,
            month,
            day,
            hour: within.div_euclid(SECONDS_PER_HOUR),
            minute: within
                .rem_euclid(SECONDS_PER_HOUR)
                .div_euclid(SECONDS_PER_MINUTE),
            second: within.rem_euclid(SECONDS_PER_MINUTE),
        }
    }
}

/// Days between the Unix epoch and a civil date, by Howard Hinnant's algorithm.
///
/// The era arithmetic is what makes the leap-year rule fall out instead of
/// being special-cased: four hundred years hold exactly 146,097 days.
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let year = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shift = if month > 2 { -3 } else { 9 };
    let day_of_year = 153_i64
        .checked_mul(month.checked_add(shift)?)?
        .checked_add(2)?
        .div_euclid(5)
        .checked_add(day.checked_sub(1)?)?;
    let day_of_era = year_of_era
        .checked_mul(365)?
        .checked_add(year_of_era.div_euclid(4))?
        .checked_sub(year_of_era.div_euclid(100))?
        .checked_add(day_of_year)?;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

/// How many days the month holds, leap years included.
pub(crate) fn days_in_month(year: i64, month: i64) -> Option<i64> {
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if leap => Some(29),
        2 => Some(28),
        _ => None,
    }
}

/// The civil date a day count names, inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days.saturating_add(719_468);
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = day_of_era
        .saturating_sub(day_of_era.div_euclid(1_460))
        .saturating_add(day_of_era.div_euclid(36_524))
        .saturating_sub(day_of_era.div_euclid(146_096))
        .div_euclid(365);
    let year = year_of_era.saturating_add(era.saturating_mul(400));
    let day_of_year = day_of_era.saturating_sub(
        year_of_era
            .saturating_mul(365)
            .saturating_add(year_of_era.div_euclid(4))
            .saturating_sub(year_of_era.div_euclid(100)),
    );
    let shifted_month = day_of_year
        .saturating_mul(5)
        .saturating_add(2)
        .div_euclid(153);
    let day = day_of_year
        .saturating_sub(
            153_i64
                .saturating_mul(shifted_month)
                .saturating_add(2)
                .div_euclid(5),
        )
        .saturating_add(1);
    let month = if shifted_month < 10 {
        shifted_month.saturating_add(3)
    } else {
        shifted_month.saturating_sub(9)
    };
    let year = if month <= 2 {
        year.saturating_add(1)
    } else {
        year
    };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{Civil, days_from_civil};
    use crate::time::Datetime;

    fn civil_of(text: &str) -> Civil {
        Datetime::parse_rfc3339(text).unwrap().civil()
    }

    #[test]
    fn the_epoch_reads_as_the_first_of_january() {
        assert_eq!(
            Datetime::from_seconds(0).civil(),
            Civil {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
    }

    #[test]
    fn a_wall_clock_reads_field_by_field() {
        assert_eq!(
            civil_of("2026-08-28T14:37:09Z"),
            Civil {
                year: 2026,
                month: 8,
                day: 28,
                hour: 14,
                minute: 37,
                second: 9
            }
        );
    }

    #[test]
    fn an_instant_before_the_epoch_reads_backwards_and_not_negatively() {
        // The way truncating division fails: one second before the epoch is the
        // last second of 1969, and a truncating `/` would answer day zero of
        // January 1970 with an hour of `0` and a second of `-1`.
        let before = Datetime::from_seconds(-1).civil();
        assert_eq!(
            before,
            Civil {
                year: 1969,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                second: 59
            }
        );
    }

    #[test]
    fn the_century_rule_and_its_exception_both_hold() {
        // 1900 is not a leap year and 2000 is. A calendar that gets this wrong
        // is right for 99 years at a time, which is why it is asserted rather
        // than assumed.
        assert_eq!(civil_of("1900-03-01T00:00:00Z").day, 1);
        assert_eq!(civil_of("1900-02-28T00:00:00Z").month, 2);
        assert_eq!(civil_of("2000-02-29T12:00:00Z").day, 29);
        assert_eq!(civil_of("2000-02-29T12:00:00Z").month, 2);
    }

    #[test]
    fn every_day_of_four_centuries_reads_back_to_the_day_it_was_written() {
        // The inversion check the two halves need against each other: without
        // it, a shared error in `days_from_civil` and `civil_from_days` cancels
        // out in a round trip and neither test notices. Walking the day counter
        // instead means the *dates* are the thing compared, and a slip of one
        // day anywhere in 146,097 of them shows up as a mismatch.
        let mut day = days_from_civil(1800, 1, 1).unwrap();
        let last = days_from_civil(2200, 1, 1).unwrap();
        let mut previous = Datetime::from_seconds(day.saturating_mul(86_400)).civil();
        while day < last {
            day = day.saturating_add(1);
            let today = Datetime::from_seconds(day.saturating_mul(86_400)).civil();
            assert_eq!(
                days_from_civil(today.year, today.month, today.day),
                Some(day),
                "{today:?} does not read back to its own day count"
            );
            let stepped = if today.day == 1 {
                today.month != previous.month
            } else {
                today.day == previous.day.saturating_add(1)
            };
            assert!(stepped, "{previous:?} was not followed by {today:?}");
            previous = today;
        }
    }
}
