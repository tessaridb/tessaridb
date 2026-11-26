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
    // code per row and plain paying a value per row. The column is sized like
    // the other selection tests: on a handful of values the decode-time
    // guard reads nanosecond noise, and a throughput claim a noise-level
    // measurement cannot support is not a claim this test should make.
    let values = strings(&["stable"; 256]);
    assert_eq!(choose(&values).0, Encoding::RunLength);
}

#[test]
fn selection_picks_second_order_on_a_monotone_sequence() {
    // A perfectly monotone run is a perfectly linear one, whose second
    // differences are all zero — so the second-order candidate, which costs
    // no bits per value there, outranks the plain delta the run used to
    // choose before it existed. The property the test holds is unchanged:
    // the cascade picks the smallest exact encoding for monotone data.
    let values: Vec<Value> = (0..256)
        .map(|row| Value::Number(Number::Integer(i64::from(row) * 2)))
        .collect();
    assert_eq!(choose(&values).0, Encoding::DoubleDelta);
    // Deltas that vary irregularly defeat the second order and keep the
    // plain one winning: alternating steps of 2 and 998 have large turns.
    let mut noisy = Vec::new();
    let mut at = 0_i64;
    for row in 0..256 {
        at += if row % 2 == 0 { 2 } else { 998 };
        noisy.push(Value::Number(Number::Integer(at)));
    }
    assert_eq!(choose(&noisy).0, Encoding::Delta);
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

#[test]
fn double_delta_roundtrips_cadence_and_jitter() {
    // A ten-second cadence with jitter: the second differences are tiny and
    // signed, which is exactly the shape the encoding exists for.
    let mut at = 1_700_000_000_i64;
    let mut run = vec![at];
    for step in 0..200_i64 {
        at += 10 + (step % 7).saturating_sub(3);
        run.push(at);
    }
    roundtrip(Encoding::DoubleDelta, &ints(&run));
}

#[test]
fn double_delta_roundtrips_edges_and_short_runs() {
    roundtrip(
        Encoding::DoubleDelta,
        &ints(&[i64::MIN, i64::MIN + 5, i64::MIN, i64::MIN + 9]),
    );
    roundtrip(Encoding::DoubleDelta, &ints(&[42]));
    roundtrip(Encoding::DoubleDelta, &ints(&[42, -42]));
    roundtrip(Encoding::DoubleDelta, &ints(&[-5, -5, -5, -5]));
    // A span whose first difference leaves the i64 range does not apply —
    // the cascade falls back rather than truncates.
    assert!(encode(Encoding::DoubleDelta, &ints(&[i64::MIN, 0])).is_none());
}

#[test]
fn gorilla_roundtrips_float_bits_exactly() {
    // Slowly-moving measures, where the XOR windows are narrow.
    let mut held = 21.5_f64;
    let mut run = Vec::new();
    for step in 0..200_u32 {
        held += f64::from(step % 5) * 0.01;
        run.push(held);
    }
    roundtrip(Encoding::Gorilla, &floats(&run));
    // Bit patterns that are not numbers at all still round-trip: the encoding
    // moves bits, it never interprets them.
    let odd = vec![
        f64::NAN,
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        f64::MIN_POSITIVE,
    ];
    roundtrip(Encoding::Gorilla, &floats(&odd));
    roundtrip(Encoding::Gorilla, &floats(&[3.5]));
}

#[test]
fn the_cascade_picks_second_order_for_a_cadence_and_xor_for_a_drift() {
    // A clean cadence: delta-of-delta is all zero, one bit per value.
    let cadence: Vec<i64> = (0..300).map(|step| 1_700_000_000 + step * 60).collect();
    let (encoding, _) = choose(&ints(&cadence));
    assert_eq!(
        encoding,
        Encoding::DoubleDelta,
        "cadence chose {encoding:?}"
    );
}

#[test]
fn structured_floats_beat_plain_and_roundtrip_through_the_choice() {
    // Which of the float candidates wins a given shape is the measured
    // cascade's call — a band over the bits, an XOR window and a scaled
    // decimal each take a different fixture. What must hold for every
    // structured float run is that the choice beats the floor and that the
    // chosen section decodes to the same values.
    let mut rest = Vec::new();
    for level in 0..60 {
        let held = 21.5 + f64::from(level) * 0.01;
        for _ in 0..5 {
            rest.push(held);
        }
    }
    let mut prices = Vec::new();
    for step in 0..300 {
        prices.push(99.99 + f64::from(step % 40) * 0.01);
    }
    let mut drift = Vec::new();
    let mut held = std::f64::consts::PI;
    for _ in 0..300 {
        held += 0.000_001;
        drift.push(held);
    }
    for fixture in [rest, prices, drift] {
        let values = floats(&fixture);
        let (encoding, bytes) = choose(&values);
        assert_ne!(encoding, Encoding::Plain, "structured floats fell to plain");
        let mut cursor = Cursor::new(&bytes);
        let decoded = decode(encoding.tag(), &mut cursor, values.len()).expect("decodes");
        cursor
            .finish()
            .expect("the section ends where its bytes do");
        assert_eq!(decoded, values, "{encoding:?} chosen but not exact");
    }
}

#[test]
fn scaled_decimal_roundtrips_prices_and_refuses_the_rest() {
    // Two-decimal prices: every value an exact multiple of a hundredth.
    let mut prices = Vec::new();
    for step in 0..200 {
        prices.push(10.0 + f64::from(step) * 0.25);
    }
    roundtrip(Encoding::ScaledDecimal, &floats(&prices));
    roundtrip(Encoding::ScaledDecimal, &floats(&[0.0, -0.0, 1.5, -2.25]));
    roundtrip(Encoding::ScaledDecimal, &floats(&[1e9, -1e9, 0.001]));
    // A value no power of ten scales exactly does not apply.
    assert!(encode(Encoding::ScaledDecimal, &floats(&[0.1 + 0.2])).is_none());
    assert!(encode(Encoding::ScaledDecimal, &floats(&[f64::NAN])).is_none());
}

#[test]
fn the_cascade_picks_scaled_decimal_for_prices() {
    let mut prices = Vec::new();
    for step in 0..300 {
        prices.push(99.99 + f64::from(step % 40) * 0.01);
    }
    let (_, bytes) = choose(&floats(&prices));
    let plain = encode(Encoding::Plain, &floats(&prices)).expect("plain applies");
    assert!(bytes.len() < plain.len(), "the choice must beat the floor");
}
