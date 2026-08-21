//! Reading a value from the text a person writes.
//!
//! These live with the types rather than with the query language on purpose.
//! Every front door — the language, the REST interface, the console — has to
//! read an instant and a UUID from text, and two readers for one literal
//! disagree eventually. When they do, the disagreement is a value that differs
//! by an hour depending on which door it came through, and nothing anywhere
//! raises an error.
//!
//! Both readers refuse rather than repair. A text that is nearly a datetime is
//! not silently rounded, clamped or reinterpreted, because every one of those
//! produces a stored value the author did not write.

use crate::time::Datetime;

/// Seconds in the units the wall-clock fields carry.
const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = 86_400;
/// The most sub-second digits the type can hold.
const NANOS_DIGITS: usize = 9;

impl Datetime {
    /// Read an instant from RFC 3339 text: `1970-01-01T00:00:00Z`.
    ///
    /// A numeric offset is accepted and converted, because the value it names
    /// is unambiguous and doing the subtraction here is one implementation
    /// rather than one per caller. The stored instant is UTC either way — this
    /// type has no zone, and a zone is a rendering choice made where the value
    /// is displayed.
    ///
    /// Returns `None` for anything else: a field out of range, a day that does
    /// not exist in its month, a leap second, or more sub-second digits than
    /// the type carries. Each of those has a plausible repair, and every repair
    /// stores a moment the author did not write.
    #[must_use]
    pub fn parse_rfc3339(text: &str) -> Option<Self> {
        let year = digits(text.get(0..4)?)?;
        if text.get(4..5)? != "-" {
            return None;
        }
        let month = digits(text.get(5..7)?)?;
        if text.get(7..8)? != "-" {
            return None;
        }
        let day = digits(text.get(8..10)?)?;
        if !text.get(10..11)?.eq_ignore_ascii_case("t") {
            return None;
        }
        let hour = digits(text.get(11..13)?)?;
        if text.get(13..14)? != ":" {
            return None;
        }
        let minute = digits(text.get(14..16)?)?;
        if text.get(16..17)? != ":" {
            return None;
        }
        let second = digits(text.get(17..19)?)?;

        // A leap second is refused rather than folded onto :59, which would
        // give one moment two spellings that do not compare equal.
        if day < 1 || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        // The month is checked by the lookup itself, which has no answer for a
        // thirteenth month.
        if day > days_in_month(year, month)? {
            return None;
        }

        let (nanos, zone) = fraction(text.get(19..)?)?;
        let offset = offset_seconds(zone)?;

        let days = days_from_civil(year, month, day)?;
        let seconds = days
            .checked_mul(SECONDS_PER_DAY)?
            .checked_add(hour.checked_mul(SECONDS_PER_HOUR)?)?
            .checked_add(minute.checked_mul(SECONDS_PER_MINUTE)?)?
            .checked_add(second)?
            .checked_sub(offset)?;
        Self::new(seconds, nanos)
    }
}

/// Read sixteen bytes from a UUID's text form.
///
/// Accepts the canonical `8-4-4-4-12` form and the same digits with no hyphens,
/// and nothing else. Hyphens anywhere else are refused rather than skipped: a
/// reader that ignores them accepts text that is not a UUID and produces bytes
/// that no other reader will agree with.
#[must_use]
pub fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    const HYPHENS: [usize; 4] = [8, 13, 18, 23];

    let plain = if text.len() == 36 {
        for position in HYPHENS {
            if text.get(position..position.saturating_add(1))? != "-" {
                return None;
            }
        }
        text.replace('-', "")
    } else {
        text.to_owned()
    };
    if plain.len() != 32 {
        return None;
    }

    let mut bytes = [0_u8; 16];
    for (index, pair) in plain.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_value(*pair.first()?)?;
        let low = hex_value(*pair.get(1)?)?;
        *bytes.get_mut(index)? = high.checked_mul(16)?.checked_add(low)?;
    }
    Some(bytes)
}

/// The sub-second digits, and whatever follows them.
fn fraction(tail: &str) -> Option<(u32, &str)> {
    let Some(rest) = tail.strip_prefix('.') else {
        return Some((0, tail));
    };
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    let written = rest.get(0..end)?;
    // More digits than the type holds are refused, not truncated: truncating
    // stores a moment earlier than the one written.
    if written.is_empty() || written.len() > NANOS_DIGITS {
        return None;
    }
    let mut padded = written.to_owned();
    while padded.len() < NANOS_DIGITS {
        padded.push('0');
    }
    let nanos = u32::try_from(digits(&padded)?).ok()?;
    Some((nanos, rest.get(end..)?))
}

/// How far the written zone sits from UTC.
fn offset_seconds(zone: &str) -> Option<i64> {
    if zone.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = match zone.get(0..1)? {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let body = zone.get(1..)?;
    if body.len() != 5 || body.get(2..3)? != ":" {
        return None;
    }
    let hours = digits(body.get(0..2)?)?;
    let minutes = digits(body.get(3..5)?)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    hours
        .checked_mul(SECONDS_PER_HOUR)?
        .checked_add(minutes.checked_mul(SECONDS_PER_MINUTE)?)?
        .checked_mul(sign)
}

/// Days between the Unix epoch and a civil date, by Howard Hinnant's algorithm.
///
/// The era arithmetic is what makes the leap-year rule fall out instead of
/// being special-cased: four hundred years hold exactly 146,097 days.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
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
fn days_in_month(year: i64, month: i64) -> Option<i64> {
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if leap => Some(29),
        2 => Some(28),
        _ => None,
    }
}

/// A run of ASCII digits as a number, or nothing.
fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// One hexadecimal digit's value.
const fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit.saturating_sub(b'0')),
        b'a'..=b'f' => Some(digit.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(digit.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_epoch_reads_as_the_origin() {
        let epoch = Datetime::parse_rfc3339("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(epoch, Datetime::from_seconds(0));
    }

    #[test]
    fn a_date_reads_as_the_moment_it_names() {
        // 2026-09-01 is 20,697 days after the epoch; the era arithmetic has to
        // land exactly, and being one leap day out is the way it fails.
        let instant = Datetime::parse_rfc3339("2026-09-01T00:00:00Z").unwrap();
        assert_eq!(instant.seconds(), 20_697 * SECONDS_PER_DAY);

        let noon = Datetime::parse_rfc3339("2026-09-01T12:30:15Z").unwrap();
        assert_eq!(
            noon.seconds(),
            20_697 * SECONDS_PER_DAY + 12 * SECONDS_PER_HOUR + 30 * SECONDS_PER_MINUTE + 15
        );
    }

    #[test]
    fn a_date_before_the_epoch_reads_as_a_negative_offset() {
        let instant = Datetime::parse_rfc3339("1969-12-31T23:59:59Z").unwrap();
        assert_eq!(instant.seconds(), -1);
    }

    #[test]
    fn a_leap_day_exists_only_in_a_leap_year() {
        assert!(Datetime::parse_rfc3339("2024-02-29T00:00:00Z").is_some());
        assert!(Datetime::parse_rfc3339("2000-02-29T00:00:00Z").is_some());
        // Refused rather than rolled into March, which is what a reader that
        // does not check the month length does.
        assert!(Datetime::parse_rfc3339("2026-02-29T00:00:00Z").is_none());
        assert!(Datetime::parse_rfc3339("1900-02-29T00:00:00Z").is_none());
    }

    #[test]
    fn an_offset_is_converted_rather_than_stored() {
        let with_offset = Datetime::parse_rfc3339("2026-09-01T03:00:00+03:00").unwrap();
        let utc = Datetime::parse_rfc3339("2026-09-01T00:00:00Z").unwrap();
        assert_eq!(with_offset, utc);

        let behind = Datetime::parse_rfc3339("2026-08-31T21:00:00-03:00").unwrap();
        assert_eq!(behind, utc);
    }

    #[test]
    fn sub_second_digits_are_padded_and_never_truncated() {
        assert_eq!(
            Datetime::parse_rfc3339("1970-01-01T00:00:00.5Z")
                .unwrap()
                .nanos(),
            500_000_000
        );
        assert_eq!(
            Datetime::parse_rfc3339("1970-01-01T00:00:00.000000001Z")
                .unwrap()
                .nanos(),
            1
        );
        // Ten digits would have to lose one, and losing it stores an earlier
        // moment than the one written.
        assert!(Datetime::parse_rfc3339("1970-01-01T00:00:00.0000000001Z").is_none());
    }

    #[test]
    fn text_that_is_not_an_instant_is_refused() {
        for bad in [
            "",
            "1970-01-01",
            "1970-01-01T00:00:00",
            "1970-13-01T00:00:00Z",
            "1970-00-01T00:00:00Z",
            "1970-01-32T00:00:00Z",
            "1970-01-01T24:00:00Z",
            "1970-01-01T00:60:00Z",
            "1970-01-01T00:00:60Z",
            "1970-01-01T00:00:00+3:00",
            "1970-01-01T00:00:00+0300",
            "1970/01/01T00:00:00Z",
            "yesterday",
        ] {
            assert!(Datetime::parse_rfc3339(bad).is_none(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_uuid_reads_in_both_of_its_written_forms() {
        let expected = [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ];
        assert_eq!(
            parse_uuid("550e8400-e29b-41d4-a716-446655440000"),
            Some(expected)
        );
        assert_eq!(
            parse_uuid("550E8400-E29B-41D4-A716-446655440000"),
            Some(expected)
        );
        assert_eq!(
            parse_uuid("550e8400e29b41d4a716446655440000"),
            Some(expected)
        );
    }

    #[test]
    fn text_that_is_not_a_uuid_is_refused() {
        for bad in [
            "",
            "550e8400",
            "550e8400-e29b-41d4-a716-44665544000",
            "550e8400e29b41d4a71644665544000g",
            "550e-8400-e29b-41d4-a716-4466554400",
            "550e8400--e29b-41d4-a716-44665544000",
        ] {
            assert!(parse_uuid(bad).is_none(), "{bad} was accepted");
        }
    }
}
