use super::*;

#[test]
fn a_present_record_round_trips_its_payload() {
    let value = RecordValue::Present(b"payload".to_vec());
    let encoded = value.encode();
    assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
    assert_eq!(RecordValue::decode(encoded.as_slice()).unwrap(), value);
}

#[test]
fn a_tombstone_is_a_version_with_no_payload() {
    let encoded = RecordValue::Tombstone.encode();
    assert_eq!(encoded.as_slice(), &[CODEC_VERSION, FLAG_TOMBSTONE]);
    let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
    assert!(decoded.is_tombstone());
    assert!(decoded.payload().is_empty());
}

#[test]
fn an_empty_present_record_is_not_a_tombstone() {
    let encoded = RecordValue::Present(Vec::new()).encode();
    let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
    assert!(!decoded.is_tombstone());
}

#[test]
fn an_unknown_codec_version_is_refused_rather_than_guessed() {
    let error = RecordValue::decode(&[9, 0]).unwrap_err();
    assert!(matches!(
        error,
        Error::UnsupportedCodecVersion {
            found: 9,
            supported: CODEC_VERSION
        }
    ));
    assert_eq!(error.code(), "incompatible");
}

#[test]
fn a_reserved_flag_bit_is_refused() {
    let error = RecordValue::decode(&[CODEC_VERSION, 0b0000_0010]).unwrap_err();
    assert!(matches!(error, Error::ReservedFlags { flags: 0b10 }));
}

#[test]
fn a_tombstone_bit_on_a_meta_value_is_reserved_there() {
    let error = FormatVersion::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0, 0, 0, 1]).unwrap_err();
    assert!(matches!(error, Error::ReservedFlags { .. }));
}

#[test]
fn a_tombstone_carrying_payload_contradicts_itself() {
    let error = RecordValue::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0xAA]).unwrap_err();
    assert!(matches!(error, Error::TombstoneWithPayload { len: 1 }));
}

#[test]
fn a_value_shorter_than_its_header_is_truncated() {
    assert!(matches!(
        RecordValue::decode(&[CODEC_VERSION]).unwrap_err(),
        Error::ValueTruncated { len: 1, needed: 2 }
    ));
}

#[test]
fn the_format_version_round_trips_and_refuses_newer_stores() {
    let encoded = FormatVersion::CURRENT.encode();
    assert_eq!(
        FormatVersion::decode(encoded.as_slice()).unwrap(),
        FormatVersion::CURRENT
    );
    assert!(FormatVersion::CURRENT.check_supported().is_ok());
    // Derived from CURRENT rather than written as a literal: a version this
    // test restates is a version this test stops checking the moment the
    // format moves.
    match FormatVersion::new(99).check_supported().unwrap_err() {
        Error::UnsupportedFormatVersion { found, supported } => {
            assert_eq!(found, 99);
            assert_eq!(supported, FormatVersion::CURRENT.get());
        }
        other => panic!("expected an unsupported-format error, got {other:?}"),
    }
}

#[test]
fn a_sequence_round_trips_as_a_stored_value() {
    let sequence = Sequence::new(1_234_567);
    let encoded = sequence.encode();
    assert_eq!(Sequence::decode(encoded.as_slice()).unwrap(), sequence);
}
