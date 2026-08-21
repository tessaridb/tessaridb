//! What the value system promises.
//!
//! The ordering checks are **exhaustive over a curated corpus** rather than
//! random. A total order fails on specific pairs — a number against an infinity,
//! a zero against a negative zero, an empty array against a missing one — and a
//! generator is unlikely to produce those while an author choosing the corpus
//! puts them in on purpose. Every pair is checked for antisymmetry and every
//! triple for transitivity, so nothing in the corpus escapes.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use bgv_db_types::{Datetime, Duration, Number, RecordId, RecordRef, TableId, Value, ValueRange};
use rust_decimal::Decimal;

/// One of each type, plus the values that break a careless implementation.
fn corpus() -> Vec<Value> {
    vec![
        Value::None,
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        Value::Number(Number::Integer(i64::MIN)),
        Value::Number(Number::Integer(-1)),
        Value::Number(Number::Integer(0)),
        Value::Number(Number::Integer(1)),
        Value::Number(Number::Integer(i64::MAX)),
        Value::Number(Number::float(-0.0)),
        Value::Number(Number::float(0.5)),
        Value::Number(Number::float(1.5)),
        Value::Number(Number::float(f64::INFINITY)),
        Value::Number(Number::float(f64::NEG_INFINITY)),
        Value::Number(Number::float(f64::NAN)),
        Value::Number(Number::float(f64::MAX)),
        Value::Number(Number::Decimal(Decimal::ZERO)),
        Value::Number(Number::Decimal(Decimal::ONE)),
        Value::String(String::new()),
        Value::String("a".to_owned()),
        Value::String("ab".to_owned()),
        Value::Bytes(Vec::new()),
        Value::Bytes(vec![0x00]),
        Value::Duration(Duration::new(-1, 500_000_000).unwrap()),
        Value::Duration(Duration::from_seconds(0)),
        Value::Duration(Duration::new(0, 1).unwrap()),
        Value::Datetime(Datetime::from_seconds(-1)),
        Value::Datetime(Datetime::from_seconds(0)),
        Value::Uuid([0x00; 16]),
        Value::Uuid([0xff; 16]),
        Value::Table(TableId::new(1)),
        Value::Table(TableId::new(2)),
        Value::Record(RecordRef::new(TableId::new(1), RecordId::from("a"))),
        Value::Record(RecordRef::new(TableId::new(1), RecordId::Int(1))),
        Value::Array(Vec::new()),
        Value::Array(vec![Value::Null]),
        Value::Array(vec![Value::Null, Value::None]),
        Value::Object(BTreeMap::new()),
        Value::Object(BTreeMap::from([("k".to_owned(), Value::Null)])),
        Value::Range(Box::new(ValueRange::new(
            Bound::Unbounded,
            Bound::Unbounded,
        ))),
        Value::Range(Box::new(ValueRange::new(
            Bound::Included(Value::from(1_i64)),
            Bound::Excluded(Value::from(9_i64)),
        ))),
        Value::Set(BTreeSet::new()),
        Value::Set(BTreeSet::from([Value::Null])),
    ]
}

// ------------------------------------------------------------- the type set

#[test]
fn the_corpus_covers_every_type_in_the_milestone_set_and_no_other() {
    let mut seen: Vec<&'static str> = corpus().iter().map(Value::type_name).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen,
        [
            "array", "bool", "bytes", "datetime", "duration", "none", "null", "number", "object",
            "range", "record", "set", "string", "table", "uuid",
        ],
        "the value set is fixed at fifteen types by the milestone scope"
    );
    assert_eq!(seen.len(), 15);
}

// ------------------------------------------------------- absent versus null

#[test]
fn absent_and_null_are_different_values_in_every_operation() {
    assert_ne!(Value::None, Value::Null);
    assert!(Value::None < Value::Null, "absence sorts before nothing");
    assert!(!Value::None.is_present());
    assert!(
        Value::Null.is_present(),
        "null is a value that says nothing, not the absence of one"
    );
    assert_eq!(Value::None.type_name(), "none");
    assert_eq!(Value::Null.type_name(), "null");
}

// ---------------------------------------------------------------- numbers

#[test]
fn a_number_is_one_value_whichever_kind_it_arrives_as() {
    let integer = Value::Number(Number::Integer(1));
    let float = Value::Number(Number::float(1.0));
    let decimal = Value::Number(Number::Decimal(Decimal::ONE));
    let trailing_zeros = Value::Number(Number::Decimal(Decimal::new(100, 2)));

    assert_eq!(integer, float);
    assert_eq!(float, decimal);
    assert_eq!(decimal, trailing_zeros, "1.00 is 1");
}

#[test]
fn numbers_compare_by_magnitude_and_not_by_kind() {
    // The whole point of one shared rank: ranking kinds apart would make this
    // comparison false while every test that used one kind still passed.
    assert!(Value::from(1_i64) < Value::from(1.5_f64));
    assert!(Value::from(1.5_f64) < Value::from(2_i64));
    assert!(Value::Number(Number::Decimal(Decimal::new(15, 1))) > Value::from(1_i64));
}

#[test]
fn negative_zero_is_zero() {
    assert_eq!(Number::float(-0.0), Number::float(0.0));
    assert_eq!(Number::float(-0.0), Number::Integer(0));
}

#[test]
fn the_non_finite_floats_have_declared_places() {
    let low = Number::float(f64::NEG_INFINITY);
    let high = Number::float(f64::INFINITY);
    let undefined = Number::float(f64::NAN);
    let ordinary = Number::Integer(0);
    let huge = Number::float(f64::MAX);

    assert!(low < ordinary, "negative infinity is below every number");
    assert!(ordinary < high);
    assert!(
        huge < high,
        "a finite float, however large, is below infinity"
    );
    assert!(high < undefined, "not-a-number sits above infinity");
    assert!(
        low < huge,
        "a float beyond decimal range still orders by its sign"
    );
}

#[test]
fn every_not_a_number_is_the_same_not_a_number() {
    let one = Number::float(f64::NAN);
    let other = Number::float(-f64::NAN);
    assert_eq!(one, other);
    assert!(one.is_nan());
}

#[test]
fn equal_numbers_hash_equally_whatever_kind_they_are() {
    use std::collections::HashSet;

    let mut set = HashSet::new();
    set.insert(Value::Number(Number::Integer(1)));
    set.insert(Value::Number(Number::float(1.0)));
    set.insert(Value::Number(Number::Decimal(Decimal::ONE)));
    assert_eq!(set.len(), 1, "three spellings of one number are one entry");
}

// ------------------------------------------------------ the order is total

#[test]
fn comparison_is_antisymmetric_over_every_pair() {
    let values = corpus();
    for left in &values {
        for right in &values {
            let forward = left.cmp(right);
            let backward = right.cmp(left);
            assert_eq!(
                forward,
                backward.reverse(),
                "{left} vs {right}: {forward:?} does not mirror {backward:?}"
            );
            assert_eq!(
                forward == core::cmp::Ordering::Equal,
                left == right,
                "{left} vs {right}: ordering and equality disagree"
            );
        }
    }
}

#[test]
fn comparison_is_transitive_over_every_triple() {
    let values = corpus();
    for a in &values {
        for b in &values {
            if a > b {
                continue;
            }
            for c in &values {
                if b > c {
                    continue;
                }
                assert!(a <= c, "{a} <= {b} <= {c} but not {a} <= {c}");
            }
        }
    }
}

#[test]
fn sorting_the_corpus_is_stable_whatever_order_it_starts_in() {
    let mut forward = corpus();
    let mut reversed = corpus();
    reversed.reverse();
    forward.sort();
    reversed.sort();
    assert_eq!(forward, reversed);
}

#[test]
fn every_type_orders_below_the_next_one_in_the_declared_rank() {
    // The rank order is contract, not accident. Reordering it reorders every
    // index holding a mixed column, so it is pinned here.
    let one_of_each = [
        Value::None,
        Value::Null,
        Value::Bool(false),
        Value::from(0_i64),
        Value::from(""),
        Value::Bytes(Vec::new()),
        Value::Duration(Duration::default()),
        Value::Datetime(Datetime::default()),
        Value::Uuid([0; 16]),
        Value::Table(TableId::new(0)),
        Value::Record(RecordRef::new(TableId::new(0), RecordId::Int(0))),
        Value::Array(Vec::new()),
        Value::Object(BTreeMap::new()),
        Value::Range(Box::new(ValueRange::new(
            Bound::Unbounded,
            Bound::Unbounded,
        ))),
        Value::Set(BTreeSet::new()),
    ];
    for pair in one_of_each.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{} must rank below {}",
            pair[0].type_name(),
            pair[1].type_name()
        );
    }
}

// ------------------------------------------------------------- containers

#[test]
fn an_object_keeps_its_fields_in_name_order_so_equal_content_is_equal() {
    let one = Value::Object(BTreeMap::from([
        ("b".to_owned(), Value::from(2_i64)),
        ("a".to_owned(), Value::from(1_i64)),
    ]));
    let other = Value::Object(BTreeMap::from([
        ("a".to_owned(), Value::from(1_i64)),
        ("b".to_owned(), Value::from(2_i64)),
    ]));
    assert_eq!(one, other);
}

#[test]
fn a_set_holds_one_of_each_value_even_across_numeric_kinds() {
    let set = BTreeSet::from([
        Value::Number(Number::Integer(1)),
        Value::Number(Number::float(1.0)),
        Value::Number(Number::Decimal(Decimal::ONE)),
        Value::Number(Number::Integer(2)),
    ]);
    assert_eq!(set.len(), 2);
}

#[test]
fn values_nest_without_a_size_problem() {
    let nested = Value::Array(vec![Value::Object(BTreeMap::from([(
        "range".to_owned(),
        Value::Range(Box::new(ValueRange::new(
            Bound::Included(Value::from(1_i64)),
            Bound::Unbounded,
        ))),
    )]))]);
    assert_eq!(nested, nested.clone());
}
