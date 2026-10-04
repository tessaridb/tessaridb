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
        0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00,
        0x00,
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
        0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44, 0x00,
        0x00,
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
