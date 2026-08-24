//! Ordering properties of the key encodings.
//!
//! The one guarantee these encodings exist to provide is that **byte order
//! equals logical order**. An encoder that round-trips perfectly and sorts
//! wrongly passes every round-trip test and then returns the wrong rows from
//! every range scan, silently. These tests assert the ordering directly, over a
//! swept domain rather than a handful of chosen values.

#![allow(clippy::unwrap_used)]

use std::cmp::Ordering;

use proptest::prelude::*;
use tessari_encoding::{KeyKind, KeyReader, KeyWriter, RecordKey, StoreKey};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

fn encode_i64(value: i64) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_i64(value);
    writer.finish()
}

fn encode_u32(value: u32) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_u32(value);
    writer.finish()
}

fn encode_u64(value: u64) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_u64(value);
    writer.finish()
}

fn encode_u64_descending(value: u64) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_u64_descending(value);
    writer.finish()
}

fn encode_variable(value: &[u8]) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_variable(value);
    writer.finish()
}

/// Values at and around every boundary that has ever broken an integer encoding.
const I64_BOUNDARIES: &[i64] = &[
    i64::MIN,
    i64::MIN + 1,
    -4_294_967_297,
    -4_294_967_296,
    -65_537,
    -256,
    -2,
    -1,
    0,
    1,
    2,
    255,
    256,
    65_535,
    65_536,
    4_294_967_295,
    4_294_967_296,
    i64::MAX - 1,
    i64::MAX,
];

fn record_id_strategy() -> impl Strategy<Value = RecordId> {
    prop_oneof![
        any::<i64>().prop_map(RecordId::Int),
        // Include the bytes that the escaping scheme treats specially.
        proptest::string::string_regex("[\\x00-\\x7f]{0,12}")
            .unwrap()
            .prop_map(RecordId::Text),
        any::<[u8; 16]>().prop_map(RecordId::Uuid),
        proptest::collection::vec(any::<u8>(), 0..12).prop_map(RecordId::Bytes),
    ]
}

#[test]
fn signed_integer_boundaries_encode_in_numeric_order() {
    for pair in I64_BOUNDARIES.windows(2) {
        assert!(
            encode_i64(pair[0]) < encode_i64(pair[1]),
            "{} must encode below {}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn the_variable_encoding_handles_the_bytes_it_treats_specially() {
    // 0x00 is the escape byte, 0x01 the terminator's second byte, 0xff the
    // escaped-zero marker. Every one of them appearing as content is the case
    // an escaping scheme gets wrong.
    let cases: &[&[u8]] = &[
        b"",
        &[0x00],
        &[0x00, 0x00],
        &[0x00, 0x01],
        &[0x00, 0xff],
        &[0x01],
        &[0xff],
        &[0xff, 0xff],
        &[0x00, 0xff, 0x00, 0x01],
    ];
    for case in cases {
        let encoded = encode_variable(case);
        let mut reader = KeyReader::new(KeyKind::Record, &encoded);
        assert_eq!(reader.take_variable().unwrap(), case.to_vec());
        reader.finish().unwrap();
    }

    let mut sorted: Vec<&&[u8]> = cases.iter().collect();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[0] == pair[1] {
            continue;
        }
        assert!(
            encode_variable(pair[0]) < encode_variable(pair[1]),
            "{:?} must encode below {:?}",
            pair[0],
            pair[1]
        );
    }
}

proptest! {
    #[test]
    fn unsigned_integers_preserve_order(left: u32, right: u32) {
        prop_assert_eq!(encode_u32(left).cmp(&encode_u32(right)), left.cmp(&right));
    }

    #[test]
    fn wide_unsigned_integers_preserve_order(left: u64, right: u64) {
        prop_assert_eq!(encode_u64(left).cmp(&encode_u64(right)), left.cmp(&right));
    }

    #[test]
    fn signed_integers_preserve_order(left: i64, right: i64) {
        prop_assert_eq!(encode_i64(left).cmp(&encode_i64(right)), left.cmp(&right));
    }

    #[test]
    fn descending_integers_reverse_order(left: u64, right: u64) {
        prop_assert_eq!(
            encode_u64_descending(left).cmp(&encode_u64_descending(right)),
            right.cmp(&left)
        );
    }

    #[test]
    fn variable_components_preserve_content_order(
        left in proptest::collection::vec(any::<u8>(), 0..24),
        right in proptest::collection::vec(any::<u8>(), 0..24),
    ) {
        prop_assert_eq!(
            encode_variable(&left).cmp(&encode_variable(&right)),
            left.cmp(&right)
        );
    }

    #[test]
    fn variable_components_round_trip(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let encoded = encode_variable(&bytes);
        let mut reader = KeyReader::new(KeyKind::Record, &encoded);
        prop_assert_eq!(reader.take_variable().unwrap(), bytes);
        prop_assert!(reader.finish().is_ok());
    }

    #[test]
    fn record_keys_round_trip(
        namespace: u32,
        database: u32,
        table: u32,
        id in record_id_strategy(),
        version: u64,
    ) {
        let key = RecordKey::new(
            NamespaceId::new(namespace),
            DatabaseId::new(database),
            TableId::new(table),
            id,
            Sequence::new(version),
        );
        let encoded = key.encode();
        prop_assert_eq!(RecordKey::decode(encoded.as_slice()).unwrap(), key);
    }

    #[test]
    fn record_keys_sort_by_tenancy_then_identity_then_newest_version_first(
        left_table: u32,
        right_table: u32,
        left_id in record_id_strategy(),
        right_id in record_id_strategy(),
        left_version: u64,
        right_version: u64,
    ) {
        let build = |table: u32, id: RecordId, version: u64| {
            RecordKey::new(
                NamespaceId::new(1),
                DatabaseId::new(1),
                TableId::new(table),
                id,
                Sequence::new(version),
            )
        };
        let left = build(left_table, left_id.clone(), left_version);
        let right = build(right_table, right_id.clone(), right_version);

        // The logical order the layout claims: table ascending, then record id
        // ascending, then version DESCENDING so the newest version is first.
        let expected = left_table
            .cmp(&right_table)
            .then_with(|| left_id.cmp(&right_id))
            .then_with(|| right_version.cmp(&left_version));

        let actual = left.encode().as_slice().cmp(right.encode().as_slice());
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn a_record_key_always_starts_with_its_table_prefix(
        namespace: u32,
        database: u32,
        table: u32,
        id in record_id_strategy(),
        version: u64,
    ) {
        let namespace = NamespaceId::new(namespace);
        let database = DatabaseId::new(database);
        let table = TableId::new(table);
        let key = RecordKey::new(namespace, database, table, id.clone(), Sequence::new(version));

        let table_prefix = RecordKey::table_prefix(namespace, database, table);
        let versions_prefix = RecordKey::versions_prefix(namespace, database, table, &id);
        let encoded = key.encode();

        prop_assert!(encoded.as_slice().starts_with(&table_prefix));
        prop_assert!(encoded.as_slice().starts_with(&versions_prefix));
        prop_assert_eq!(encoded.len(), versions_prefix.len().saturating_add(8));
    }

    #[test]
    fn two_different_record_ids_never_encode_to_the_same_key(
        left in record_id_strategy(),
        right in record_id_strategy(),
    ) {
        let build = |id: RecordId| {
            RecordKey::new(
                NamespaceId::new(1),
                DatabaseId::new(1),
                TableId::new(1),
                id,
                Sequence::ZERO,
            )
            .encode()
        };
        if left == right {
            prop_assert_eq!(build(left), build(right));
        } else {
            prop_assert_ne!(build(left), build(right));
        }
    }
}

#[test]
fn ordering_is_a_total_order_over_a_mixed_key_set() {
    // A sanity net over the properties above: build a varied set, sort it by
    // encoded bytes, and assert the decoded sequence is non-decreasing in the
    // logical order the layout promises.
    let mut keys = Vec::new();
    for table in [1_u32, 2] {
        for id in [
            RecordId::Int(i64::MIN),
            RecordId::Int(-1),
            RecordId::Int(0),
            RecordId::Int(i64::MAX),
            RecordId::from(""),
            RecordId::from("a"),
            RecordId::from("ab"),
            RecordId::from("b"),
            RecordId::Uuid([0x00; 16]),
            RecordId::Uuid([0xff; 16]),
            RecordId::Bytes(vec![0x00]),
            RecordId::Bytes(vec![0xff]),
        ] {
            for version in [0_u64, 1, u64::MAX] {
                keys.push(RecordKey::new(
                    NamespaceId::new(1),
                    DatabaseId::new(1),
                    TableId::new(table),
                    id.clone(),
                    Sequence::new(version),
                ));
            }
        }
    }

    let mut encoded: Vec<Vec<u8>> = keys.iter().map(|key| key.encode().into_bytes()).collect();
    encoded.sort_unstable();
    encoded.dedup();
    assert_eq!(encoded.len(), keys.len(), "two distinct keys collided");

    let decoded: Vec<RecordKey> = encoded
        .iter()
        .map(|bytes| RecordKey::decode(bytes).unwrap())
        .collect();

    for pair in decoded.windows(2) {
        let logical = pair[0]
            .table
            .cmp(&pair[1].table)
            .then_with(|| pair[0].id.cmp(&pair[1].id))
            .then_with(|| pair[1].version.cmp(&pair[0].version));
        assert_eq!(
            logical,
            Ordering::Less,
            "byte order disagrees with logical order at {:?} / {:?}",
            pair[0],
            pair[1]
        );
    }
}
