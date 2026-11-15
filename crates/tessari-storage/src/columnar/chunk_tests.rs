//! Chunk-level round-trips, null handling, the record-id index, and the
//! hostile-input contract.
#![allow(clippy::unwrap_used)]

use tessari_types::{Number, RecordId, Value};

use super::chunk::{decode_chunk, encode_chunk};

fn int(value: i64) -> Value {
    Value::Number(Number::Integer(value))
}

/// Encode and decode, expecting identical rows, ids, and null count.
fn roundtrip(ids: &[RecordId], values: &[Value]) {
    let bytes = encode_chunk(ids, values);
    let chunk = decode_chunk(&bytes).expect("a fresh chunk always decodes");
    assert_eq!(chunk.record_ids, ids);
    assert_eq!(chunk.values, values, "values must round-trip exactly");
    let nulls = values
        .iter()
        .filter(|v| matches!(v, Value::Null | Value::None))
        .count();
    assert_eq!(chunk.null_count, nulls);
}

fn sequential_ids(count: usize) -> Vec<RecordId> {
    (0..count)
        .map(|row| RecordId::Int(i64::try_from(row).expect("test ids are small")))
        .collect()
}

#[test]
fn a_mixed_run_roundtrips() {
    let ids = sequential_ids(6);
    let values = vec![
        int(1),
        Value::Null,
        int(2),
        int(2),
        Value::Null,
        Value::Bool(true),
    ];
    roundtrip(&ids, &values);
}

#[test]
fn every_record_id_kind_roundtrips() {
    let ids = vec![
        RecordId::Int(7),
        RecordId::Text("sensor-a".to_owned()),
        RecordId::Uuid([0xab; 16]),
        RecordId::Bytes(vec![1, 2, 3]),
    ];
    let values = vec![int(1), int(2), int(3), int(4)];
    roundtrip(&ids, &values);
}

#[test]
fn an_all_null_chunk_has_an_empty_data_section() {
    let ids = sequential_ids(4);
    let values = vec![Value::Null; 4];
    let bytes = encode_chunk(&ids, &values);
    // Header(10) + bitmap(1) + the empty plain section(4) + the record-id
    // index(56: four integer ids at 13 bytes each plus its length) — 71 in
    // all. The same chunk holding four integer values would add their data
    // (~56 bytes); the all-null chunk's data section stays at its floor.
    assert!(
        bytes.len() < 100,
        "an all-null chunk's data section is its empty floor: {}",
        bytes.len()
    );
    roundtrip(&ids, &values);
}

#[test]
fn a_zero_null_chunk_takes_the_branch_free_path() {
    let ids = sequential_ids(4);
    let values = vec![int(1), int(2), int(3), int(4)];
    let bytes = encode_chunk(&ids, &values);
    let chunk = decode_chunk(&bytes).expect("a fresh chunk always decodes");
    assert_eq!(chunk.null_count, 0);
    assert_eq!(chunk.values, values);
}

#[test]
fn none_maps_to_a_null_slot() {
    // A schemaless record that does not mention the field is covered by a
    // null slot (D-SG2-2); the decoded chunk answers `Null` for it, and the
    // row path stays the authority on the distinction.
    let ids = sequential_ids(2);
    let values = vec![Value::None, int(1)];
    let bytes = encode_chunk(&ids, &values);
    let chunk = decode_chunk(&bytes).expect("a fresh chunk always decodes");
    assert_eq!(chunk.values, vec![Value::Null, int(1)]);
    assert_eq!(chunk.null_count, 1);
}

#[test]
fn sorted_is_recorded_for_an_append_ordered_run() {
    let ids = sequential_ids(5);
    let values = vec![int(1), int(2), int(2), int(3), int(9)];
    let bytes = encode_chunk(&ids, &values);
    let chunk = decode_chunk(&bytes).expect("a fresh chunk always decodes");
    assert!(chunk.sorted);
}

#[test]
fn sorted_is_not_recorded_for_a_scattered_run() {
    let ids = sequential_ids(3);
    let values = vec![int(3), int(1), int(2)];
    let bytes = encode_chunk(&ids, &values);
    let chunk = decode_chunk(&bytes).expect("a fresh chunk always decodes");
    assert!(!chunk.sorted);
}

#[test]
fn nan_does_not_break_the_sorted_flag_contract() {
    // NaN has a declared place in the total order; the flag is computed
    // under that order and the values round-trip bit-exactly.
    let ids = sequential_ids(3);
    let values = vec![
        Value::Number(Number::float(f64::NAN)),
        Value::Number(Number::float(1.0)),
        Value::Number(Number::float(f64::NAN)),
    ];
    let bytes = encode_chunk(&ids, &values);
    let chunk = decode_chunk(&bytes).expect("a fresh chunk always decodes");
    assert_eq!(chunk.values, values);
}

#[test]
fn every_truncation_is_refused() {
    let ids = sequential_ids(8);
    let values = vec![
        int(1),
        Value::Null,
        int(2),
        int(3),
        Value::Null,
        int(5),
        int(8),
        int(13),
    ];
    let bytes = encode_chunk(&ids, &values);
    for cut in 0..bytes.len() {
        assert!(
            decode_chunk(&bytes[..cut]).is_err(),
            "a chunk cut to {cut} bytes must refuse, not guess"
        );
    }
}

#[test]
fn an_unknown_encoding_tag_is_refused() {
    let ids = sequential_ids(2);
    let values = vec![int(1), int(2)];
    let mut bytes = encode_chunk(&ids, &values);
    bytes[4] = 200; // the encoding tag sits right after the count
    assert!(matches!(
        decode_chunk(&bytes),
        Err(crate::error::Error::ColumnarUnknownEncoding { found: 200 })
    ));
}

#[test]
fn a_bitmap_that_disagrees_with_the_count_is_refused() {
    let ids = sequential_ids(4);
    let values = vec![int(1), Value::Null, int(2), Value::Null];
    let mut bytes = encode_chunk(&ids, &values);
    // Clear one validity bit: the bitmap now counts one null, the header two.
    // The bitmap is the first byte after count+encoding+flags+null_count.
    let bitmap_at = 4 + 1 + 1 + 4;
    bytes[bitmap_at] = 0;
    assert!(decode_chunk(&bytes).is_err());
}

#[test]
fn an_absurd_row_count_is_refused_before_it_allocates() {
    let ids = sequential_ids(1);
    let values = vec![int(1)];
    let mut bytes = encode_chunk(&ids, &values);
    bytes[0] = 0xff;
    bytes[1] = 0xff;
    bytes[2] = 0xff;
    bytes[3] = 0xff; // count = u32::MAX, far past the row ceiling
    assert!(decode_chunk(&bytes).is_err());
}
