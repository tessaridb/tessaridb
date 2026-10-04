// Test assertions are exactly where a panic is the correct outcome; the
// lints below target production paths.
#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;

fn variable(value: &[u8]) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_variable(value);
    writer.finish()
}

#[test]
fn variable_components_match_the_documented_vectors() {
    // These are the worked cases in `docs/key-grammar.md` §4.3.
    assert_eq!(variable(b"a"), vec![0x61, 0x00, 0x01]);
    assert_eq!(variable(b"ab"), vec![0x61, 0x62, 0x00, 0x01]);
    assert_eq!(variable(b"a\x00"), vec![0x61, 0x00, 0xFF, 0x00, 0x01]);
    assert_eq!(variable(b""), vec![0x00, 0x01]);
}

#[test]
fn a_shorter_component_sorts_before_a_longer_one_extending_it() {
    assert!(variable(b"a") < variable(b"ab"));
    assert!(variable(b"a") < variable(b"a\x00"));
    assert!(variable(b"") < variable(b"a"));
}

#[test]
fn signed_integers_put_negatives_first() {
    let encode = |value: i64| {
        let mut writer = KeyWriter::new();
        writer.put_i64(value);
        writer.finish()
    };
    assert_eq!(encode(i64::MIN), vec![0x00; 8]);
    assert_eq!(encode(i64::MAX), vec![0xFF; 8]);
    assert!(encode(-1) < encode(0));
    assert!(encode(0) < encode(1));
    assert!(encode(i64::MIN) < encode(-1));
}

#[test]
fn descending_u64_reverses_order() {
    let encode = |value: u64| {
        let mut writer = KeyWriter::new();
        writer.put_u64_descending(value);
        writer.finish()
    };
    assert!(encode(9) < encode(1));
    assert_eq!(encode(0), vec![0xFF; 8]);
    assert_eq!(encode(u64::MAX), vec![0x00; 8]);
}

#[test]
fn a_truncated_read_names_the_kind_and_the_offset() {
    let mut reader = KeyReader::new(KeyKind::Record, &[0x01, 0x02]);
    let error = reader.take_u32().unwrap_err();
    match error {
        Error::Truncated {
            kind,
            offset,
            needed,
            available,
        } => {
            assert_eq!(kind, KeyKind::Record);
            assert_eq!(offset, 0);
            assert_eq!(needed, 4);
            assert_eq!(available, 2);
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn an_unterminated_component_is_rejected() {
    let mut reader = KeyReader::new(KeyKind::Record, b"abc");
    assert!(matches!(
        reader.take_variable().unwrap_err(),
        Error::UnterminatedComponent { .. }
    ));
}

#[test]
fn an_invalid_escape_is_rejected_with_its_offset() {
    let mut reader = KeyReader::new(KeyKind::Record, &[0x61, 0x00, 0x07]);
    match reader.take_variable().unwrap_err() {
        Error::InvalidEscape {
            offset,
            found,
            kind,
            ..
        } => {
            assert_eq!(kind, KeyKind::Record);
            assert_eq!(offset, 1);
            assert_eq!(found, 0x07);
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_wrong_kind_tag_names_what_was_found() {
    let bytes = [KeyKind::LogEntry.tag()];
    let mut reader = KeyReader::new(KeyKind::Record, &bytes);
    match reader.expect_kind().unwrap_err() {
        Error::UnexpectedKind {
            expected,
            found,
            found_name,
        } => {
            assert_eq!(expected, KeyKind::Record);
            assert_eq!(found, KeyKind::LogEntry.tag());
            assert_eq!(found_name, "log-entry");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn an_unassigned_tag_is_reported_as_unassigned() {
    let mut reader = KeyReader::new(KeyKind::Record, &[0x7F]);
    match reader.expect_kind().unwrap_err() {
        Error::UnexpectedKind { found_name, .. } => assert_eq!(found_name, "unassigned"),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn trailing_bytes_are_a_decode_failure() {
    let mut reader = KeyReader::new(KeyKind::Record, &[0x00, 0x00, 0x00, 0x01, 0xAA]);
    assert_eq!(reader.take_u32().unwrap(), 1);
    assert!(matches!(
        reader.finish().unwrap_err(),
        Error::TrailingBytes { extra: 1, .. }
    ));
}

#[test]
fn every_primitive_round_trips() {
    let mut writer = KeyWriter::with_capacity(64);
    writer
        .put_u8(7)
        .put_u32(u32::MAX)
        .put_u64(1 << 40)
        .put_i64(-9)
        .put_u64_descending(5)
        .put_variable(b"with\x00zero")
        .put_fixed(&[1, 2, 3]);
    let bytes = writer.finish();

    let mut reader = KeyReader::new(KeyKind::Record, &bytes);
    assert_eq!(reader.take_u8().unwrap(), 7);
    assert_eq!(reader.take_u32().unwrap(), u32::MAX);
    assert_eq!(reader.take_u64().unwrap(), 1 << 40);
    assert_eq!(reader.take_i64().unwrap(), -9);
    assert_eq!(reader.take_u64_descending().unwrap(), 5);
    assert_eq!(reader.take_variable().unwrap(), b"with\x00zero".to_vec());
    assert_eq!(reader.take_fixed::<3>().unwrap(), [1, 2, 3]);
    reader.finish().unwrap();
}
