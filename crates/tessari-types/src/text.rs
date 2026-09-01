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

use core::fmt::Write as _;

use crate::calendar::{
    SECONDS_PER_DAY, SECONDS_PER_HOUR, SECONDS_PER_MINUTE, days_from_civil, days_in_month,
};
use crate::time::Datetime;

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

/// Write sixteen bytes in a UUID's canonical `8-4-4-4-12` text form.
///
/// The inverse of [`parse_uuid`], and it lives beside it for the reason this
/// module's header gives for the readers: two writers for one literal disagree
/// eventually.
///
/// `Value`'s `Display` writes `uuid:` and thirty-two undivided digits, and that
/// is a *rendering* — tagged so a reader of a log cannot mistake it for a
/// string, and never read back. This is the form a person writes and every
/// other system reads, so it is the one a cast to text produces: that text is a
/// value, and a value gets stored, sent, and read again.
#[must_use]
pub fn uuid_to_text(bytes: &[u8; 16]) -> String {
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        // Two digits together, so a byte below sixteen keeps its leading zero —
        // the difference between a UUID and a shorter string that looks like
        // one. Writing into a `String` has no failure to handle.
        let _infallible = write!(text, "{byte:02x}");
    }
    text
}

/// Write text as the language's single-quoted string literal.
///
/// Beside [`uuid_to_text`] for the reason that one gives: two writers for one
/// literal disagree eventually. This one had two candidates and one writer — the
/// console renders values in TessariQL and had the only escaper, privately,
/// while the record-id spelling needs the same one and cannot reach into a
/// binary crate. The choice was to write a second or to move this; a second
/// escaper differs on the first character somebody forgets.
#[must_use]
pub fn string_to_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('\'');
    for character in text.chars() {
        match character {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
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
    fn a_uuid_round_trips_through_its_canonical_text() {
        // The property the reader and the writer share, asserted in both
        // directions — the same one the instant has above, and for the same
        // reason: a writer that was never checked against the reader is how a
        // value comes back as something else.
        for bytes in [
            [0_u8; 16],
            [0xff; 16],
            [
                0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
                0x00, 0x00,
            ],
            // A leading zero in the first byte, which a writer that formats
            // without a width silently drops.
            [
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
                0x0f, 0x00,
            ],
        ] {
            let text = uuid_to_text(&bytes);
            assert_eq!(text.len(), 36, "{text}");
            assert_eq!(parse_uuid(&text), Some(bytes), "{text}");
        }
    }

    #[test]
    fn the_canonical_form_is_the_one_everybody_writes() {
        let bytes = [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ];
        assert_eq!(uuid_to_text(&bytes), "550e8400-e29b-41d4-a716-446655440000");
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

impl Datetime {
    /// Write an instant as RFC 3339 text: `1970-01-01T00:00:00Z`.
    ///
    /// The inverse of [`Datetime::parse_rfc3339`], and it lives beside it for
    /// the reason this module gives for the reader: every front door has to turn
    /// an instant into text, and two writers for one literal disagree
    /// eventually. That the reader was written here and the writer was not is
    /// how the console came to print `0.000000000` where an instant belonged.
    ///
    /// Always `Z`, because this type has no zone — a zone is a rendering choice,
    /// and the one this store makes is to render the moment it stored.
    ///
    /// Sub-second digits appear only when there are any, so an instant on a
    /// whole second reads the way anybody writes one.
    #[must_use]
    pub fn to_rfc3339(self) -> String {
        // The date is read by `crate::calendar`, not derived here. A second copy
        // of the era arithmetic beside the first is how a writer comes to
        // disagree with its own reader on one day in four hundred years.
        let civil = self.civil();
        let (year, month, day) = (civil.year, civil.month, civil.day);
        let (hour, minute, second) = (civil.hour, civil.minute, civil.second);
        let mut text = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
        if self.nanos() > 0 {
            // Trailing zeros trimmed: `.5` and `.500000000` name one instant,
            // and the shorter is the one a person writes.
            let fraction = format!("{:09}", self.nanos());
            text.push('.');
            text.push_str(fraction.trim_end_matches('0'));
        }
        text.push('Z');
        text
    }
}

impl crate::time::Duration {
    /// Write a span in the language's own duration syntax: `1h30m`, `2s`, `-500ms`.
    ///
    /// Not [`core::fmt::Display`], which writes `5400.000000000s` — a debugging
    /// form that the language's lexer will not read back, because a duration
    /// there is digits touching a letter and carries no fraction.
    ///
    /// A zero span is `0s`, since an empty string is not a literal.
    #[must_use]
    pub fn to_literal(self) -> String {
        // The magnitude is written and the sign prefixed, so the units below do
        // not each have to know about it.
        //
        // Below zero the two fields are a FLOOR pair — negative seconds with a
        // POSITIVE remainder added to them — so -500ms is (-1, 500_000_000) and
        // its magnitude is one second less than the seconds', with the
        // remainder taken the other way round. Reading the fields
        // independently spells `-1s500ms` for that span, which is a different
        // duration and loses another second every time it is read back.
        let negative = self.seconds() < 0;
        let (mut whole, nanos) = if negative && self.nanos() > 0 {
            // Neither of these can saturate, and both are written that way
            // rather than bare: the branch requires seconds below zero, so the
            // magnitude is at least one, and the remainder is below a second by
            // the type's own construction rule.
            (
                self.seconds().unsigned_abs().saturating_sub(1),
                1_000_000_000_u32.saturating_sub(self.nanos()),
            )
        } else {
            (self.seconds().unsigned_abs(), self.nanos())
        };
        let mut text = String::new();
        if negative {
            text.push('-');
        }
        for (unit, size) in [("h", 3_600_u64), ("m", 60)] {
            let held = whole.div_euclid(size);
            if held > 0 {
                text.push_str(&format!("{held}{unit}"));
                whole = whole.rem_euclid(size);
            }
        }
        if whole > 0 {
            text.push_str(&format!("{whole}s"));
        }
        if nanos > 0 {
            // Milliseconds, microseconds and nanoseconds in turn, so a remainder
            // is written in the largest unit that holds it exactly.
            let mut remainder = nanos;
            for (unit, size) in [("ms", 1_000_000_u32), ("us", 1_000), ("ns", 1)] {
                let held = remainder.div_euclid(size);
                if held > 0 {
                    text.push_str(&format!("{held}{unit}"));
                    remainder = remainder.rem_euclid(size);
                }
            }
        }
        if text.is_empty() || text == "-" {
            text.push_str("0s");
        }
        text
    }
}

#[cfg(test)]
mod writing {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use crate::time::{Datetime, Duration};

    #[test]
    fn an_instant_round_trips_through_its_own_text() {
        // The property the module exists for, now asserted in both directions:
        // the reader was written here and the writer was not, which is how the
        // console came to print `0.000000000` where an instant belonged.
        for (seconds, nanos) in [
            (0_i64, 0_u32),
            (1, 0),
            (1_000_000_000, 0),
            (-1, 0),
            (-86_400, 0),
            (1_755_000_000, 123_456_789),
            (951_782_400, 500_000_000),
        ] {
            let held = Datetime::new(seconds, nanos).expect("an instant");
            let text = held.to_rfc3339();
            let again = Datetime::parse_rfc3339(&text)
                .unwrap_or_else(|| panic!("{text:?} did not read back"));
            assert_eq!(held, again, "{text}");
        }
    }

    #[test]
    fn the_epoch_is_written_the_way_everybody_writes_it() {
        let epoch = Datetime::new(0, 0).expect("the epoch");
        assert_eq!(epoch.to_rfc3339(), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_leap_day_survives_both_directions() {
        // The date arithmetic's own edge, and the one a wrong inverse gets
        // wrong: 2000 is a leap year and 1900 is not.
        let text = "2000-02-29T12:00:00Z";
        let held = Datetime::parse_rfc3339(text).expect("a leap day");
        assert_eq!(held.to_rfc3339(), text);
    }

    #[test]
    fn a_fraction_keeps_only_the_digits_it_has() {
        let half = Datetime::new(0, 500_000_000).expect("half a second");
        assert_eq!(half.to_rfc3339(), "1970-01-01T00:00:00.5Z");
        let precise = Datetime::new(0, 123_456_789).expect("nine digits");
        assert_eq!(precise.to_rfc3339(), "1970-01-01T00:00:00.123456789Z");
    }

    #[test]
    fn a_duration_is_written_in_the_language_s_own_units() {
        // `Display` writes `5400.000000000s`, which the lexer will not read: a
        // duration there is digits touching a letter and carries no fraction.
        let cases: &[(i64, u32, &str)] = &[
            (0, 0, "0s"),
            (2, 0, "2s"),
            (5_400, 0, "1h30m"),
            (3_600, 0, "1h"),
            (90, 0, "1m30s"),
            (-2, 0, "-2s"),
            (0, 500_000_000, "500ms"),
            (0, 1, "1ns"),
            (0, 1_500, "1us500ns"),
            (61, 250_000_000, "1m1s250ms"),
        ];
        for (seconds, nanos, expected) in cases {
            let held = Duration::new(*seconds, *nanos).expect("a duration");
            assert_eq!(held.to_literal(), *expected, "{seconds}s {nanos}ns");
        }
    }

    #[test]
    fn a_negative_span_is_written_as_its_own_magnitude() {
        // A negative span is a FLOOR pair: the seconds are below zero and the
        // remainder is a positive addend, so -500ms is (-1, 500_000_000) and
        // its magnitude is one second LESS than the seconds' magnitude, with
        // the remainder counted the other way round.
        //
        // Writing the two fields independently — `-`, then |seconds|, then the
        // remainder — spells a different span, and the table above never caught
        // it because its only negative rows carry no remainder, where the floor
        // pair and the magnitude pair happen to agree.
        let cases: &[(i64, u32, &str)] = &[
            (-1, 500_000_000, "-500ms"),   // -0.5s
            (-1, 999_999_999, "-1ns"),     // the smallest span below zero
            (-1, 1, "-999ms999us999ns"),   // the carry reaches every unit
            (-3, 500_000_000, "-2s500ms"), // -2.5s
            (-5_401, 500_000_000, "-1h30m500ms"),
            (-1, 0, "-1s"), // no remainder: unchanged, and still correct
            (-3_600, 0, "-1h"),
        ];
        for (seconds, nanos, expected) in cases {
            let held = Duration::new(*seconds, *nanos).expect("a duration");
            assert_eq!(held.to_literal(), *expected, "{seconds}s {nanos}ns");
        }
    }
}
