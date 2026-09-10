//! What a record payload survives.
//!
//! Round-trips are checked over the same shape of corpus the value system's own
//! tests use — one of each type plus the awkward cases — because a codec that
//! only ever sees the easy values is a codec whose first production input is its
//! first real test.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use rust_decimal::Decimal;
use tessari_encoding::{Error, decode_payload, encode_payload};
use tessari_types::{Datetime, Duration, Number, RecordId, RecordRef, TableId, Value, ValueRange};

fn corpus() -> Vec<Value> {
    vec![
        Value::None,
        Value::Null,
        Value::Bool(true),
        Value::Bool(false),
        Value::Number(Number::Integer(i64::MIN)),
        Value::Number(Number::Integer(0)),
        Value::Number(Number::Integer(i64::MAX)),
        Value::Number(Number::float(1.5)),
        Value::Number(Number::float(f64::INFINITY)),
        Value::Number(Number::float(f64::NEG_INFINITY)),
        Value::Number(Number::float(f64::NAN)),
        Value::Number(Number::float(-0.0)),
        Value::Number(Number::Decimal(Decimal::ZERO)),
        Value::Number(Number::Decimal(Decimal::new(-12_345, 3))),
        Value::Number(Number::Decimal(Decimal::MAX)),
        Value::Number(Number::Decimal(Decimal::MIN)),
        Value::String(String::new()),
        Value::String("ordinary".to_owned()),
        Value::String("двоичное \u{1f600} \u{0}".to_owned()),
        Value::Bytes(Vec::new()),
        Value::Bytes(vec![0x00, 0xff, 0x00, 0x01]),
        Value::Duration(Duration::new(-1, 999_999_999).unwrap()),
        Value::Duration(Duration::from_seconds(i64::MAX)),
        Value::Datetime(Datetime::new(i64::MIN, 0).unwrap()),
        Value::Datetime(Datetime::new(1_700_000_000, 123).unwrap()),
        Value::Uuid([0x00; 16]),
        Value::Uuid([0xff; 16]),
        Value::Table(TableId::new(u32::MAX)),
        Value::Record(RecordRef::new(TableId::new(7), RecordId::from("a:b"))),
        Value::Record(RecordRef::new(TableId::new(7), RecordId::Int(i64::MIN))),
        Value::Record(RecordRef::new(TableId::new(7), RecordId::Uuid([0x5a; 16]))),
        Value::Record(RecordRef::new(
            TableId::new(7),
            RecordId::Bytes(vec![0x00, 0x01]),
        )),
        Value::Array(Vec::new()),
        Value::Array(vec![Value::None, Value::Null, Value::from(1_i64)]),
        Value::Object(BTreeMap::new()),
        Value::Object(BTreeMap::from([
            ("empty name is legal".to_owned(), Value::Null),
            (String::new(), Value::from("x")),
        ])),
        Value::Range(Box::new(ValueRange::new(
            Bound::Unbounded,
            Bound::Unbounded,
        ))),
        Value::Range(Box::new(ValueRange::new(
            Bound::Included(Value::from(1_i64)),
            Bound::Excluded(Value::from("z")),
        ))),
        Value::Set(BTreeSet::new()),
        Value::Set(BTreeSet::from([Value::Null, Value::from(2_i64)])),
    ]
}

#[test]
fn every_value_survives_a_round_trip() {
    for value in corpus() {
        let encoded = encode_payload(&value);
        let decoded = decode_payload(encoded.as_slice()).unwrap();
        assert_eq!(decoded, value, "round trip changed {value}");
    }
}

#[test]
fn nesting_survives_a_round_trip() {
    let deep = Value::Object(BTreeMap::from([(
        "outer".to_owned(),
        Value::Array(vec![
            Value::Set(BTreeSet::from([Value::from(1_i64)])),
            Value::Range(Box::new(ValueRange::new(
                Bound::Included(Value::Object(BTreeMap::from([(
                    "inner".to_owned(),
                    Value::Bytes(vec![0xde, 0xad]),
                )]))),
                Bound::Unbounded,
            ))),
        ]),
    )]));
    let encoded = encode_payload(&deep);
    assert_eq!(decode_payload(encoded.as_slice()).unwrap(), deep);
}

#[test]
fn absent_and_null_encode_to_different_bytes() {
    // The distinction has to survive storage, or it is not a distinction.
    let absent = encode_payload(&Value::None);
    let null = encode_payload(&Value::Null);
    assert_ne!(absent.as_slice(), null.as_slice());
    assert_eq!(decode_payload(absent.as_slice()).unwrap(), Value::None);
    assert_eq!(decode_payload(null.as_slice()).unwrap(), Value::Null);
}

#[test]
fn an_unknown_type_tag_is_refused_and_names_the_tag() {
    let error = decode_payload(&[0xee]).unwrap_err();
    assert!(
        matches!(error, Error::UnknownValueTag { tag: 0xee }),
        "{error}"
    );
    assert_eq!(
        error.code(),
        "incompatible",
        "well-formed bytes from a newer build are not corruption"
    );
}

#[test]
fn an_unknown_number_kind_is_refused() {
    // Type tag 0x04 is a number; 0x7f is not one of its three kinds.
    let error = decode_payload(&[0x04, 0x7f]).unwrap_err();
    assert!(
        matches!(error, Error::UnknownValueTag { tag: 0x7f }),
        "{error}"
    );
}

#[test]
fn a_payload_cut_anywhere_inside_a_value_is_refused() {
    for value in corpus() {
        let encoded = encode_payload(&value);
        let bytes = encoded.as_slice();
        for cut in 0..bytes.len() {
            assert!(
                decode_payload(&bytes[..cut]).is_err(),
                "{value} cut at {cut} decoded anyway"
            );
        }
    }
}

#[test]
fn trailing_bytes_after_a_value_are_refused() {
    // A payload that decodes and leaves bytes over is not the value it claims
    // to be — something else is in there.
    let mut bytes = encode_payload(&Value::Null).into_bytes();
    bytes.push(0x00);
    assert!(matches!(
        decode_payload(&bytes).unwrap_err(),
        Error::TrailingBytes { .. }
    ));
}

#[test]
fn text_that_is_not_utf8_is_refused_rather_than_replaced() {
    // A lossy conversion would store a different string than was written and
    // report success.
    let mut bytes = encode_payload(&Value::String("ok".to_owned())).into_bytes();
    let last = bytes.len().saturating_sub(1);
    bytes[last] = 0xff;
    assert!(matches!(
        decode_payload(&bytes).unwrap_err(),
        Error::InvalidUtf8 { .. }
    ));
}

#[test]
fn a_decimal_keeps_its_scale_so_two_and_two_point_zero_zero_stay_distinguishable() {
    // They compare equal as numbers, and they are still not the same bytes:
    // scale is information about how the value was written, and losing it would
    // change what a caller reads back.
    let plain = Value::Number(Number::Decimal(Decimal::new(2, 0)));
    let scaled = Value::Number(Number::Decimal(Decimal::new(200, 2)));
    assert_eq!(plain, scaled, "as numbers they are one value");

    let plain_bytes = encode_payload(&plain);
    let scaled_bytes = encode_payload(&scaled);
    assert_ne!(plain_bytes.as_slice(), scaled_bytes.as_slice());

    match decode_payload(scaled_bytes.as_slice()).unwrap() {
        Value::Number(Number::Decimal(decoded)) => assert_eq!(decoded.scale(), 2),
        other => panic!("expected a decimal, got {other}"),
    }
}

#[test]
fn a_decimal_whose_mantissa_and_scale_do_not_describe_a_number_is_refused() {
    // Scale is bounded; a payload naming an impossible one is malformed data
    // rather than a number to approximate.
    let mut bytes = encode_payload(&Value::Number(Number::Decimal(Decimal::ONE))).into_bytes();
    let scale_at = bytes.len().saturating_sub(4);
    bytes[scale_at..].copy_from_slice(&u32::MAX.to_be_bytes());
    let error = decode_payload(&bytes).unwrap_err();
    assert!(matches!(error, Error::InvalidDecimal { .. }), "{error}");
    assert_eq!(error.code(), "corruption");
}

#[test]
fn a_sub_second_remainder_of_a_whole_second_is_refused() {
    let mut bytes = encode_payload(&Value::Datetime(Datetime::from_seconds(1))).into_bytes();
    let nanos_at = bytes.len().saturating_sub(4);
    bytes[nanos_at..].copy_from_slice(&1_000_000_000_u32.to_be_bytes());
    assert!(matches!(
        decode_payload(&bytes).unwrap_err(),
        Error::InvalidSubSecond {
            nanos: 1_000_000_000
        }
    ));
}
