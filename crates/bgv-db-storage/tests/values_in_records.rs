//! A record holds a typed value, and gets it back.
//!
//! The layers below were built and tested separately: the value system knows
//! nothing about storage, and the store treats a payload as opaque bytes. This
//! is where the two meet, which is the only place a mismatch between them can
//! show up.
//!
//! The store's API is still byte-oriented on purpose. Making it take a value
//! would be the natural next step in layering, and it is deliberately not taken
//! yet — the engines above are what will decide the shape of that surface, and
//! guessing it now would mean rewriting it when they arrive.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bgv_db_encoding::{decode_payload, encode_payload};
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::{RecordAddress, Store};
use bgv_db_types::{DatabaseId, NamespaceId, Number, RecordId, Sequence, TableId, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn at(id: &str) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(id),
    )
}

fn put(store: &Store, id: &str, value: &Value) -> Sequence {
    let mut transaction = store.begin().unwrap();
    transaction.put(at(id), encode_payload(value).into_bytes());
    transaction.commit().unwrap()
}

fn get(store: &Store, id: &str) -> Option<Value> {
    let transaction = store.begin().unwrap();
    transaction
        .get(&at(id))
        .unwrap()
        .map(|bytes| decode_payload(&bytes).unwrap())
}

fn document() -> Value {
    Value::Object(BTreeMap::from([
        ("title".to_owned(), Value::from("a record")),
        ("count".to_owned(), Value::Number(Number::Integer(3))),
        ("ratio".to_owned(), Value::from(0.75_f64)),
        ("missing".to_owned(), Value::None),
        ("empty".to_owned(), Value::Null),
        (
            "tags".to_owned(),
            Value::Array(vec![Value::from("a"), Value::from("b")]),
        ),
    ]))
}

#[test]
fn a_record_returns_the_value_it_was_given() {
    let store = store();
    let original = document();
    put(&store, "r", &original);
    assert_eq!(get(&store, "r"), Some(original));
}

#[test]
fn a_field_that_is_absent_stays_absent_and_one_that_is_null_stays_null() {
    // The distinction has to survive the whole way down and back, or the store
    // cannot tell "we never looked" from "we looked and found nothing".
    let store = store();
    put(&store, "r", &document());
    let Some(Value::Object(fields)) = get(&store, "r") else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("missing"), Some(&Value::None));
    assert_eq!(fields.get("empty"), Some(&Value::Null));
}

#[test]
fn an_older_snapshot_reads_the_older_value() {
    let store = store();
    put(&store, "r", &Value::from("first"));
    let reader = store.begin().unwrap();
    put(&store, "r", &Value::from("second"));

    let seen = reader
        .get(&at("r"))
        .unwrap()
        .map(|bytes| decode_payload(&bytes).unwrap());
    assert_eq!(seen, Some(Value::from("first")));
    assert_eq!(get(&store, "r"), Some(Value::from("second")));
}

#[test]
fn a_deleted_record_reads_as_absent_and_not_as_a_none_value() {
    // A tombstone and a stored `NONE` are different facts, and the store must
    // not blur them: one says the record is gone, the other says it is there
    // holding a value that means "not present".
    let store = store();
    put(&store, "gone", &Value::from(1_i64));
    put(&store, "holds-none", &Value::None);

    let mut transaction = store.begin().unwrap();
    transaction.delete(at("gone"));
    transaction.commit().unwrap();

    assert_eq!(get(&store, "gone"), None, "a deleted record has no payload");
    assert_eq!(
        get(&store, "holds-none"),
        Some(Value::None),
        "a record holding an absent value still exists"
    );
}

#[test]
fn the_log_carries_the_value_so_a_replica_reconstructs_it() {
    let source = store();
    let original = document();
    put(&source, "r", &original);

    let replica = store();
    for (sequence, record) in source.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }
    assert_eq!(get(&replica, "r"), Some(original));
}
