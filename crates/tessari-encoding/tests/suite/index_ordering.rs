//! The index encoding's one job: bytes that sort the way values do.
//!
//! Everything here is checked **exhaustively over a corpus** rather than on
//! chosen examples, because the failure this encoding can have is not a crash.
//! A wrong byte order returns wrong rows from a range scan, a unique index
//! admits a duplicate, an equality lookup misses — and none of it raises an
//! error anywhere.
//!
//! The corpus is curated rather than generated: it carries the cases a random
//! generator would almost never produce — the two nullish values, the three
//! spellings of one number, a float beyond decimal range, a float below it,
//! both infinities, not-a-number, an empty string, a string containing the
//! escape byte, empty containers, and a container that is a prefix of another.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use rust_decimal::Decimal;
use tessari_encoding::{
    INDEX_PREFIX_LEN, IndexAddress, IndexValues, SecondaryIndexKey, StoreKey, UniqueIndexKey,
};
use tessari_types::{
    DatabaseId, Datetime, Duration, Geometry, IndexId, NamespaceId, Number, Polygon, Position,
    RecordId, RecordRef, Ring, TableId, Value, ValueRange,
};

fn number(number: Number) -> Value {
    Value::Number(number)
}

/// Every value the encoding has to place, in no particular order — the tests
/// sort it themselves.
fn corpus() -> Vec<Value> {
    vec![
        Value::None,
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        // Numbers, including three spellings of one value and the four values
        // that sit outside the finite line.
        number(Number::float(f64::NEG_INFINITY)),
        number(Number::Integer(i64::MIN)),
        number(Number::float(-1e100)),
        number(Number::Integer(-5)),
        number(Number::float(-1.5)),
        number(Number::Decimal(Decimal::new(-5, 1))),
        number(Number::Integer(0)),
        number(Number::float(0.0)),
        number(Number::Decimal(Decimal::ZERO)),
        number(Number::float(1e-300)),
        number(Number::Decimal(Decimal::new(5, 1))),
        number(Number::Integer(1)),
        number(Number::float(1.0)),
        number(Number::Decimal(Decimal::new(100, 2))),
        number(Number::float(1.5)),
        number(Number::Integer(1500)),
        number(Number::Decimal(Decimal::new(150_000, 2))),
        number(Number::Integer(i64::MAX)),
        number(Number::float(1e100)),
        number(Number::float(f64::INFINITY)),
        number(Number::float(f64::NAN)),
        Value::from(""),
        Value::from("a"),
        Value::from("ab"),
        Value::from("a\u{0}"),
        Value::Bytes(vec![]),
        Value::Bytes(vec![0x00]),
        Value::Bytes(vec![0x00, 0xff]),
        Value::Duration(Duration::from_seconds(-1)),
        Value::Duration(Duration::from_seconds(0)),
        Value::Duration(Duration::new(1, 500).unwrap()),
        Value::Datetime(Datetime::from_seconds(-1)),
        Value::Datetime(Datetime::from_seconds(1)),
        Value::Uuid([0x00; 16]),
        Value::Uuid([0xff; 16]),
        Value::Table(TableId::new(1)),
        Value::Table(TableId::new(2)),
        Value::Record(RecordRef::new(TableId::new(1), RecordId::from("a"))),
        Value::Record(RecordRef::new(TableId::new(1), RecordId::Int(1))),
        Value::Array(vec![]),
        Value::Array(vec![Value::from("a")]),
        Value::Array(vec![Value::from("a"), Value::from("b")]),
        Value::Object(BTreeMap::new()),
        Value::Object(BTreeMap::from([("a".to_owned(), Value::from(1_i64))])),
        Value::Object(BTreeMap::from([
            ("a".to_owned(), Value::from(1_i64)),
            ("b".to_owned(), Value::Null),
        ])),
        Value::Range(Box::new(ValueRange::new(
            Bound::Unbounded,
            Bound::Unbounded,
        ))),
        Value::Range(Box::new(ValueRange::new(
            Bound::Included(Value::from(1_i64)),
            Bound::Excluded(Value::from(5_i64)),
        ))),
        Value::Set(BTreeSet::new()),
        Value::Set(BTreeSet::from([Value::from("a")])),
        Value::Set(BTreeSet::from([Value::from("a"), Value::from("b")])),
        // --- geometry ---
        // A point, and two neighbours differing in one coordinate each, so the
        // longitude-then-latitude order is exercised in both positions.
        Value::Geometry(Geometry::Point(Position::new(0.0, 0.0))),
        Value::Geometry(Geometry::Point(Position::new(1.0, 0.0))),
        Value::Geometry(Geometry::Point(Position::new(0.0, 1.0))),
        // Negative zero. `total_cmp` puts it below zero and the bytes must too;
        // a decimal-style comparison would call them equal and a unique index
        // would then reject one of two distinct points.
        Value::Geometry(Geometry::Point(Position::new(-0.0, 0.0))),
        Value::Geometry(Geometry::Point(Position::new(-1.0, 0.0))),
        // An empty line, a line, and a line that extends it: the prefix rule.
        Value::Geometry(Geometry::Line(vec![])),
        Value::Geometry(Geometry::Line(vec![Position::new(0.0, 0.0)])),
        Value::Geometry(Geometry::Line(vec![
            Position::new(0.0, 0.0),
            Position::new(1.0, 1.0),
        ])),
        // THE case a count-prefixed encoding gets wrong. `Vec` compares
        // lexicographically, so `[a, a]` sorts before `[b]`; a leading count
        // would sort `[b]` first because one is fewer than two.
        Value::Geometry(Geometry::Line(vec![Position::new(9.0, 0.0)])),
        Value::Geometry(Geometry::Line(vec![
            Position::new(0.0, 0.0),
            Position::new(0.0, 0.0),
        ])),
        // A different variant with identical contents — the discriminant leads.
        Value::Geometry(Geometry::MultiPoint(vec![Position::new(0.0, 0.0)])),
        // Polygons: with and without a hole, so the interior sequence's
        // terminator is exercised.
        Value::Geometry(Geometry::Polygon(square(0.0))),
        Value::Geometry(Geometry::Polygon(Polygon {
            exterior: square(0.0).exterior,
            interiors: vec![square(0.5).exterior],
        })),
        Value::Geometry(Geometry::Polygon(square(1.0))),
        // Nested, so the recursion is covered rather than assumed.
        Value::Geometry(Geometry::Collection(vec![])),
        Value::Geometry(Geometry::Collection(vec![Box::new(Geometry::Point(
            Position::new(0.0, 0.0),
        ))])),
        // --- regex ---
        Value::Regex(String::new()),
        Value::Regex("a".to_owned()),
        Value::Regex("ab".to_owned()),
        // Holds the escape byte, which is what the variable-length form is for.
        Value::Regex("a\u{0}b".to_owned()),
    ]
}

/// A closed unit square with its lower-left corner at `offset`.
fn square(offset: f64) -> Polygon {
    Polygon {
        exterior: Ring(vec![
            Position::new(offset, offset),
            Position::new(offset + 1.0, offset),
            Position::new(offset + 1.0, offset + 1.0),
            Position::new(offset, offset),
        ]),
        interiors: Vec::new(),
    }
}

fn address() -> IndexAddress {
    IndexAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        IndexId::new(4),
    )
}

fn encoded(value: &Value) -> Vec<u8> {
    IndexValues::of(std::slice::from_ref(value))
        .as_slice()
        .to_vec()
}

#[test]
fn byte_order_equals_value_order_for_every_pair_in_the_corpus() {
    let corpus = corpus();
    for left in &corpus {
        for right in &corpus {
            let values = left.cmp(right);
            let bytes = encoded(left).cmp(&encoded(right));
            assert_eq!(
                values, bytes,
                "{left:?} vs {right:?}: values say {values:?}, bytes say {bytes:?}"
            );
        }
    }
}

#[test]
fn values_that_compare_equal_encode_to_identical_bytes() {
    // The pairwise test covers this, but stating it alone is what a unique
    // index actually depends on: two spellings of one number must collide.
    let ones = [
        number(Number::Integer(1)),
        number(Number::float(1.0)),
        number(Number::Decimal(Decimal::new(100, 2))),
    ];
    let first = encoded(&ones[0]);
    for value in &ones[1..] {
        assert_eq!(encoded(value), first, "{value:?} encoded differently");
    }

    let fifteen_hundreds = [
        number(Number::Integer(1500)),
        number(Number::float(1500.0)),
        number(Number::Decimal(Decimal::new(150_000, 2))),
    ];
    let first = encoded(&fifteen_hundreds[0]);
    for value in &fifteen_hundreds[1..] {
        assert_eq!(encoded(value), first, "{value:?} encoded differently");
    }
}

#[test]
fn the_corpus_covers_every_variant_the_value_system_has() {
    // A corpus that quietly lost a type would keep passing while leaving that
    // type's encoding unchecked.
    let names: BTreeSet<&'static str> = corpus().iter().map(Value::type_name).collect();
    // Seventeen since geometry and regex landed. The number is written out
    // rather than derived, because deriving it from the corpus would make this
    // test assert that the corpus equals itself.
    assert_eq!(names.len(), 17, "covered: {names:?}");
}

#[test]
fn a_secondary_key_round_trips_for_every_value_in_the_corpus() {
    for value in corpus() {
        let key = SecondaryIndexKey::new(
            address(),
            IndexValues::of(std::slice::from_ref(&value)),
            RecordId::from("r"),
        );
        let encoded = key.encode();
        assert_eq!(
            SecondaryIndexKey::decode(encoded.as_slice()).unwrap(),
            key,
            "{value:?}"
        );
    }
}

#[test]
fn a_unique_key_round_trips_and_carries_no_record_id() {
    let values = IndexValues::of(&[Value::from("ada"), Value::from(7_i64)]);
    let key = UniqueIndexKey::new(address(), values.clone());
    let encoded = key.encode();
    assert_eq!(UniqueIndexKey::decode(encoded.as_slice()).unwrap(), key);

    // Two records holding the same value produce the same unique key, which is
    // what makes the second write collide instead of sit beside the first.
    let same = UniqueIndexKey::new(address(), values);
    assert_eq!(same.encode().as_slice(), encoded.as_slice());
}

#[test]
fn two_records_with_one_value_produce_two_distinct_secondary_keys() {
    let values = IndexValues::of(&[Value::from("ada")]);
    let left = SecondaryIndexKey::new(address(), values.clone(), RecordId::from("a"));
    let right = SecondaryIndexKey::new(address(), values.clone(), RecordId::from("b"));
    assert_ne!(left.encode().as_slice(), right.encode().as_slice());

    // Both sit under the prefix a scan for that value would use.
    let prefix = SecondaryIndexKey::values_prefix(&address(), &values);
    assert!(left.encode().as_slice().starts_with(&prefix));
    assert!(right.encode().as_slice().starts_with(&prefix));
}

#[test]
fn the_index_prefix_is_fixed_width_and_leads_every_entry() {
    let prefix = address().prefix(tessari_encoding::KeyKind::SecondaryIndex);
    assert_eq!(prefix.len(), INDEX_PREFIX_LEN);
    let key = SecondaryIndexKey::new(
        address(),
        IndexValues::of(&[Value::from("x")]),
        RecordId::Int(1),
    );
    assert!(key.encode().as_slice().starts_with(&prefix));
}

#[test]
fn a_shorter_composite_sorts_before_one_that_extends_it() {
    let short = IndexValues::of(&[Value::from("a")]);
    let long = IndexValues::of(&[Value::from("a"), Value::from("b")]);
    assert!(short.as_slice() < long.as_slice());
}

#[test]
fn a_truncated_index_key_is_refused_at_every_offset() {
    let key = SecondaryIndexKey::new(
        address(),
        IndexValues::of(&[Value::from("ada"), Value::from(7_i64)]),
        RecordId::from("r"),
    );
    let encoded = key.encode();
    let bytes = encoded.as_slice();
    for cut in 1..bytes.len() {
        assert!(
            SecondaryIndexKey::decode(&bytes[..cut]).is_err(),
            "a key cut at {cut} was accepted"
        );
    }
}

#[test]
fn two_indexes_on_one_table_never_share_a_key() {
    let first = IndexAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        IndexId::new(4),
    );
    let second = IndexAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        IndexId::new(5),
    );
    let values = IndexValues::of(&[Value::from("same")]);
    assert_ne!(
        UniqueIndexKey::new(first, values.clone())
            .encode()
            .as_slice(),
        UniqueIndexKey::new(second, values).encode().as_slice()
    );
}
