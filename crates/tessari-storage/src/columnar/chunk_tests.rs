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
    // index(56: four integer ids at 13 bytes each plus its length) + the
    // stats footer's empty bloom(13: a length, the 8-byte minimum filter,
    // the hash count) — 84 in all, with min/max honestly absent (there is
    // nothing to order). The same chunk holding four integer values would
    // add their data (~56 bytes) and the min/max payloads; the all-null
    // chunk's data section stays at its empty floor.
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

// --- SG3 T3.1: footer statistics (min/max under the declared order, an
// equality bloom, the NaN rule) are written with every chunk and verified
// on read. See the SG3 plan §2 and the worksheet §4.3/§4.4.

fn float(value: f64) -> Value {
    Value::Number(Number::float(value))
}

#[test]
fn min_max_roundtrip_under_the_declared_order() {
    let ids = sequential_ids(4);
    let values = vec![int(5), int(-3), int(9), int(0)];
    let chunk = decode_chunk(&encode_chunk(&ids, &values)).expect("a fresh chunk decodes");
    assert_eq!(chunk.stats.min, Some(int(-3)));
    assert_eq!(chunk.stats.max, Some(int(9)));
}

#[test]
fn string_min_max_use_the_same_order_the_store_declares() {
    let ids = sequential_ids(3);
    let values = vec![
        Value::String("pear".to_owned()),
        Value::String("apple".to_owned()),
        Value::String("fig".to_owned()),
    ];
    let chunk = decode_chunk(&encode_chunk(&ids, &values)).expect("a fresh chunk decodes");
    assert_eq!(chunk.stats.min, Some(Value::String("apple".to_owned())));
    assert_eq!(chunk.stats.max, Some(Value::String("pear".to_owned())));
}

#[test]
fn mixed_kind_min_max_follow_the_cross_kind_rank() {
    // Numbers rank before strings in the store's one declared order, so the
    // pair spans the kinds without a second comparator anywhere (§4.3: the
    // comparator is the store's own, which is what makes the statistic
    // trustworthy for pruning).
    let ids = sequential_ids(3);
    let values = vec![
        Value::String("apple".to_owned()),
        int(1),
        Value::String("zz".to_owned()),
    ];
    let chunk = decode_chunk(&encode_chunk(&ids, &values)).expect("a fresh chunk decodes");
    assert_eq!(chunk.stats.min, Some(int(1)));
    assert_eq!(chunk.stats.max, Some(Value::String("zz".to_owned())));
}

#[test]
fn a_nan_disables_range_statistics_but_keeps_the_bloom() {
    // Worksheet §4.3: NaN is excluded from min/max and its presence disables
    // range pruning for the chunk; the bloom is unaffected and still answers
    // equality.
    let ids = sequential_ids(3);
    let values = vec![float(1.5), float(f64::NAN), float(2.5)];
    let chunk = decode_chunk(&encode_chunk(&ids, &values)).expect("a fresh chunk decodes");
    assert_eq!(chunk.stats.min, None);
    assert_eq!(chunk.stats.max, None);
    assert!(chunk.stats.range_disabled);
    let bloom = chunk.stats.bloom.expect("the bloom is written");
    let member = tessari_encoding::encode_payload(&float(2.5)).into_bytes();
    assert!(bloom.might_contain(&member));
}

#[test]
fn an_all_null_chunk_carries_no_min_max_and_an_empty_bloom() {
    let ids = sequential_ids(3);
    let values = vec![Value::Null; 3];
    let chunk = decode_chunk(&encode_chunk(&ids, &values)).expect("a fresh chunk decodes");
    assert_eq!(chunk.stats.min, None);
    assert_eq!(chunk.stats.max, None);
    let bloom = chunk.stats.bloom.expect("the bloom is written");
    // Nothing was inserted, so every probe finds an unset bit: an all-null
    // chunk is skippable for any equality predicate.
    let probe = tessari_encoding::encode_payload(&int(42)).into_bytes();
    assert!(!bloom.might_contain(&probe));
}

#[test]
fn the_bloom_answers_membership_for_every_value_it_was_built_from() {
    let ids = sequential_ids(64);
    let values: Vec<Value> = (0..64).map(|row| int(row * 7 + 3)).collect();
    let chunk = decode_chunk(&encode_chunk(&ids, &values)).expect("a fresh chunk decodes");
    let bloom = chunk.stats.bloom.expect("the bloom is written");
    for value in &values {
        let payload = tessari_encoding::encode_payload(value).into_bytes();
        assert!(
            bloom.might_contain(&payload),
            "a bloom never denies a value it holds (no false negatives)"
        );
    }
}

#[test]
fn an_absurd_bloom_length_is_refused_before_it_allocates() {
    let ids = sequential_ids(2);
    let values = vec![int(1), int(2)];
    let mut bytes = encode_chunk(&ids, &values);
    // The footer ends [bloom byte_len:u32][bloom bytes][hash_count:u8]. The
    // two-value bloom is the format's minimum (8 bytes), so the length field
    // sits 12 bytes from the end; a hostile length must refuse, not allocate.
    let at = bytes.len() - 12;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(decode_chunk(&bytes).is_err());
}
