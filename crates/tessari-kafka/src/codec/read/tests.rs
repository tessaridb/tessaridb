//! The primitive encodings, against the values the protocol specifies.

use crate::codec::{Reader, Writer};

#[test]
fn varints_round_trip_at_their_edges() {
    for value in [0, 1, -1, 63, -64, 64, i32::MAX, i32::MIN] {
        let mut writer = Writer::new();
        writer.varint(value);
        let bytes = writer.into_bytes();
        assert_eq!(Reader::new(&bytes).varint("v").unwrap(), value);
    }
    for value in [0, 1, -1, i64::MAX, i64::MIN] {
        let mut writer = Writer::new();
        writer.varlong(value);
        let bytes = writer.into_bytes();
        assert_eq!(Reader::new(&bytes).varlong("v").unwrap(), value);
    }
}

#[test]
fn a_zigzag_varint_is_the_protocols_encoding() {
    // -1 → 1, 1 → 2, -64 → 127, 64 → 128 = 0x80 0x01.
    for (value, bytes) in [
        (-1, &[0x01][..]),
        (1, &[0x02]),
        (-64, &[0x7f]),
        (64, &[0x80, 0x01]),
    ] {
        let mut writer = Writer::new();
        writer.varint(value);
        assert_eq!(writer.into_bytes(), bytes, "{value}");
    }
}

#[test]
fn an_overlong_varint_is_refused_rather_than_wrapped() {
    let bytes = [0xff_u8; 11];
    assert!(Reader::new(&bytes).uvarlong("v").is_err());
    assert!(
        Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x7f])
            .uvarint("v")
            .is_err(),
        "35 bits"
    );
}

#[test]
fn flexible_and_classic_strings_differ_only_in_their_length() {
    for flexible in [false, true] {
        let mut writer = Writer::new();
        writer.nullable_string(flexible, Some("fy")).unwrap();
        writer.nullable_string(flexible, None).unwrap();
        let bytes = writer.into_bytes();
        let expected: &[u8] = if flexible {
            &[3, b'f', b'y', 0]
        } else {
            &[0, 2, b'f', b'y', 0xff, 0xff]
        };
        assert_eq!(bytes, expected);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.nullable_string(flexible, "s").unwrap(), Some("fy"));
        assert_eq!(reader.nullable_string(flexible, "s").unwrap(), None);
    }
}

#[test]
fn a_length_past_the_frame_is_refused_before_anything_is_taken() {
    let mut reader = Reader::new(&[0x7f, 0xff, 0xff, 0xff, 1, 2]);
    assert!(reader.nullable_bytes(false, "b").is_err());
    let mut reader = Reader::new(&[0x00, 0x0f, 0x42, 0x40]);
    assert!(
        reader.array_len(false, "a").is_err(),
        "a million elements in no bytes"
    );
}

#[test]
fn unknown_tagged_fields_are_skipped() {
    // Two tags: tag 0 with two bytes, tag 5 with none; then a trailing int8.
    let bytes = [2, 0, 2, 0xaa, 0xbb, 5, 0, 7];
    let mut reader = Reader::new(&bytes);
    reader.tagged_fields("t").unwrap();
    assert_eq!(reader.i8("after").unwrap(), 7);
}
