//! Round-trips, the selection rule, and hostile input for the cascade.
#![allow(clippy::unwrap_used)]

use tessari_types::{Number, Value};

use super::codec::{Cursor, Encoding, choose, decode, encode};

/// Encode under a forced encoding, decode back, expect the identical run.
fn roundtrip(encoding: Encoding, values: &[Value]) {
    let bytes = encode(encoding, values).expect("encoding applies to the fixture");
    let mut cursor = Cursor::new(&bytes);
    let decoded =
        decode(encoding.tag(), &mut cursor, values.len()).expect("a fresh encode always decodes");
    cursor
        .finish()
        .expect("the section ends where its bytes do");
    assert_eq!(decoded, values, "{encoding:?} must round-trip exactly");
}

fn ints(values: &[i64]) -> Vec<Value> {
    values
        .iter()
        .map(|v| Value::Number(Number::Integer(*v)))
        .collect()
}

fn floats(values: &[f64]) -> Vec<Value> {
    values
        .iter()
        .map(|v| Value::Number(Number::float(*v)))
        .collect()
}

fn strings(values: &[&str]) -> Vec<Value> {
    values
        .iter()
        .map(|v| Value::String((*v).to_owned()))
        .collect()
}

#[test]
fn plain_roundtrips_mixed_values() {
    let values = vec![
        Value::Bool(true),
        Value::String("plain".to_owned()),
        Value::Number(Number::Integer(i64::MIN)),
        Value::Number(Number::Integer(i64::MAX)),
        Value::Bool(false),
    ];
    roundtrip(Encoding::Plain, &values);
}

#[test]
fn dictionary_roundtrips_low_cardinality() {
    let values = strings(&["red", "green", "red", "blue", "green", "red"]);
    roundtrip(Encoding::Dictionary, &values);
}

#[test]
fn run_length_roundtrips_runs() {
    let values = ints(&[7, 7, 7, 2, 2, 9]);
    roundtrip(Encoding::RunLength, &values);
}

#[test]
fn frame_of_reference_roundtrips_the_full_integer_range() {
    let values = ints(&[i64::MIN, i64::MIN, i64::MAX, 0, -1, i64::MAX]);
    roundtrip(Encoding::FrameOfReference, &values);
}

#[test]
fn frame_of_reference_roundtrips_float_bits_exactly() {
    // NaN, both infinities, negative zero (normalised to positive by
    // `Number::float`), and ordinary values: raw-bit packing must preserve
    // every pattern it is handed.
    let values = floats(&[1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 1.5]);
    roundtrip(Encoding::FrameOfReference, &values);
}

#[test]
fn delta_roundtrips_monotone_integers() {
    let values = ints(&[10, 10, 12, 15, 15, 100, i64::MAX - 1, i64::MAX]);
    roundtrip(Encoding::Delta, &values);
}

#[test]
fn delta_does_not_apply_to_a_non_monotone_run() {
    assert!(encode(Encoding::Delta, &ints(&[5, 3])).is_none());
    assert!(encode(Encoding::Delta, &ints(&[5, 5, 4])).is_none());
}

#[test]
fn frame_of_reference_does_not_apply_to_mixed_numerics() {
    let mixed = vec![
        Value::Number(Number::Integer(1)),
        Value::Number(Number::float(1.0)),
    ];
    assert!(encode(Encoding::FrameOfReference, &mixed).is_none());
    assert!(encode(Encoding::Delta, &mixed).is_none());
}

#[test]
fn selection_picks_dictionary_on_low_cardinality_strings() {
    let mut values = Vec::new();
    for row in 0..256 {
        values.push(Value::String(format!("colour-{}", row % 4)));
    }
    assert_eq!(choose(&values).0, Encoding::Dictionary);
}

#[test]
fn selection_picks_run_length_on_a_constant_string_column() {
    // Delta does not apply to strings, so the constant column's winner is
    // run length: one distinct value, one run — against dictionary paying a
    // code per row and plain paying a value per row.
    let values = strings(&["stable", "stable", "stable", "stable"]);
    assert_eq!(choose(&values).0, Encoding::RunLength);
}

#[test]
fn selection_picks_delta_on_a_monotone_sequence() {
    let values: Vec<Value> = (0..256)
        .map(|row| Value::Number(Number::Integer(i64::from(row) * 2)))
        .collect();
    assert_eq!(choose(&values).0, Encoding::Delta);
}

#[test]
fn selection_picks_frame_of_reference_on_clustered_non_monotone_ints() {
    let mut values = Vec::new();
    for row in 0..256 {
        let base = i64::from(row % 2) * 1000;
        values.push(Value::Number(Number::Integer(base + i64::from(row % 16))));
    }
    assert_eq!(choose(&values).0, Encoding::FrameOfReference);
}

#[test]
fn selection_picks_plain_when_nothing_beats_it() {
    // All-distinct long strings: dictionary pays the code width and the
    // distinct table with no repetition to win it back.
    let values: Vec<Value> = (0..64)
        .map(|row| Value::String(format!("a-very-distinct-string-value-{row:04}")))
        .collect();
    assert_eq!(choose(&values).0, Encoding::Plain);
}

#[test]
fn unknown_encoding_tag_is_refused_not_guessed() {
    let mut cursor = Cursor::new(&[0; 16]);
    let err = decode(99, &mut cursor, 1).unwrap_err();
    assert!(matches!(
        err,
        crate::error::Error::ColumnarUnknownEncoding { found: 99 }
    ));
}

#[test]
fn every_truncation_is_refused() {
    let values = ints(&[1, 2, 3, 4, 5]);
    let bytes = encode(Encoding::Delta, &values).expect("a monotone run encodes");
    for cut in 0..bytes.len() {
        let mut cursor = Cursor::new(&bytes[..cut]);
        assert!(
            decode(Encoding::Delta.tag(), &mut cursor, values.len()).is_err(),
            "a chunk cut to {cut} bytes must refuse, not guess"
        );
    }
}

#[test]
fn a_count_mismatch_is_refused() {
    let values = ints(&[1, 2, 3]);
    let bytes = encode(Encoding::Plain, &values).expect("plain applies to anything");
    let mut cursor = Cursor::new(&bytes);
    assert!(decode(Encoding::Plain.tag(), &mut cursor, 4).is_err());
}

#[test]
fn trailing_bytes_are_refused() {
    let values = ints(&[1, 2, 3]);
    let mut bytes = encode(Encoding::Plain, &values).expect("plain applies to anything");
    bytes.push(0);
    let mut cursor = Cursor::new(&bytes);
    assert!(decode(Encoding::Plain.tag(), &mut cursor, values.len()).is_ok());
    assert!(cursor.finish().is_err());
}
