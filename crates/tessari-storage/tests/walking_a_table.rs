//! A table walked as it is found, against the same table read whole.
//!
//! `Transaction::walk_table` exists so that a caller may stop: a read with a
//! `WHERE` cannot push its `LIMIT` into the source, because the bound counts
//! records that match and the source counts records that exist. What it has
//! instead is the consumer's `Break`, and a source that returns a `Vec` cannot
//! hear one.
//!
//! Every case here is written against [`Transaction::scan_table`] rather than
//! against a literal expectation. That is deliberate: the walk's contract is
//! not "these records" but "**the same** records, in the same order, as the
//! method that already reads this table" — and an expectation written by hand
//! would agree with whichever of the two I happened to write it from. The one
//! thing the walk may do differently is stop, and the case that asserts it says
//! so by name.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Error, RecordAddress, Store, Transaction};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

const NAMESPACE: NamespaceId = NamespaceId::new(1);
const DATABASE: DatabaseId = DatabaseId::new(1);
const TABLE: TableId = TableId::new(1);

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn at(id: &str) -> RecordAddress {
    RecordAddress::new(NAMESPACE, DATABASE, TABLE, RecordId::from(id))
}

fn payload(mark: &str) -> Vec<u8> {
    let fields = BTreeMap::from([("mark".to_owned(), Value::from(mark))]);
    encode_payload(&Value::Object(fields)).into_bytes()
}

/// What the walk hands over, in the order it hands it over.
fn walked(transaction: &mut Transaction<'_>) -> Vec<(RecordId, Vec<u8>)> {
    let mut found = Vec::new();
    transaction
        .walk_table(NAMESPACE, DATABASE, TABLE, |_, id, record| {
            found.push((id, record));
            Ok::<_, Error>(ControlFlow::Continue(()))
        })
        .unwrap();
    found
}

/// What reading the whole table hands back.
fn scanned(transaction: &Transaction<'_>) -> Vec<(RecordId, Vec<u8>)> {
    transaction.scan_table(NAMESPACE, DATABASE, TABLE).unwrap()
}

/// Enough records to need three batches, so a boundary is crossed twice.
const RECORDS: usize = RANGE_SCAN_BATCH_ENTRIES * 2 + 500;

/// A table with `RECORDS` records, a handful of them rewritten and a handful
/// deleted — so the walk meets old versions and tombstones, not only rows.
fn populated() -> Store {
    let store = store();
    let mut writing = store.begin().unwrap();
    for n in 0..RECORDS {
        writing.put(at(&format!("r{n:06}")), payload("first"));
    }
    writing.commit().unwrap();

    let mut rewriting = store.begin().unwrap();
    // One on each side of a batch boundary and one far from any, because a
    // record's versions straddling a boundary is the case `resolved` carries
    // across batches for.
    for n in [
        0,
        RANGE_SCAN_BATCH_ENTRIES - 1,
        RANGE_SCAN_BATCH_ENTRIES,
        RANGE_SCAN_BATCH_ENTRIES + 1,
        RECORDS - 1,
    ] {
        rewriting.put(at(&format!("r{n:06}")), payload("second"));
    }
    rewriting.delete(at(&format!("r{:06}", RANGE_SCAN_BATCH_ENTRIES * 2)));
    rewriting.delete(at("r000007"));
    rewriting.commit().unwrap();
    store
}

#[test]
fn the_walk_answers_exactly_what_the_scan_answers() {
    let store = populated();
    let mut transaction = store.begin().unwrap();
    let whole = scanned(&transaction);
    assert_eq!(whole.len(), RECORDS - 2, "two records were deleted");
    assert_eq!(walked(&mut transaction), whole);
}

#[test]
fn a_record_rewritten_across_a_batch_boundary_is_read_once_at_its_newest_version() {
    let store = populated();
    let mut transaction = store.begin().unwrap();
    let found = walked(&mut transaction);

    for n in [RANGE_SCAN_BATCH_ENTRIES - 1, RANGE_SCAN_BATCH_ENTRIES] {
        let id = RecordId::from(format!("r{n:06}").as_str());
        let held: Vec<_> = found.iter().filter(|(seen, _)| seen == &id).collect();
        assert_eq!(held.len(), 1, "{id} was handed over more than once");
        assert_eq!(
            held[0].1,
            payload("second"),
            "{id} came back at an old version"
        );
    }
}

#[test]
fn the_walk_stops_where_the_caller_says_stop() {
    let store = populated();
    let mut transaction = store.begin().unwrap();
    let whole = scanned(&transaction);

    let mut found = Vec::new();
    transaction
        .walk_table(NAMESPACE, DATABASE, TABLE, |_, id, record| {
            found.push((id, record));
            Ok::<_, Error>(if found.len() == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        })
        .unwrap();

    // Three, and the same three the whole read would have started with: a walk
    // that stopped early in the wrong place would also hand over three.
    assert_eq!(found, whole[..3].to_vec());
}

#[test]
fn a_record_written_in_this_transaction_arrives_in_key_order() {
    let store = populated();
    let mut transaction = store.begin().unwrap();
    // An identity that sorts into the middle of the committed records rather
    // than after all of them, which is the only arrangement that can tell a
    // merge from an append.
    transaction.put(at("r000500x"), payload("pending"));

    let found = walked(&mut transaction);
    assert_eq!(found, scanned(&transaction));

    let position = found
        .iter()
        .position(|(id, _)| id == &RecordId::from("r000500x"))
        .expect("the record written in this transaction is in the answer");
    assert_eq!(found[position].1, payload("pending"));
    assert!(found[position - 1].0 < found[position].0);
    assert!(found[position].0 < found[position + 1].0);
}

#[test]
fn a_record_deleted_in_this_transaction_is_not_handed_over() {
    let store = populated();
    let mut transaction = store.begin().unwrap();
    let before = walked(&mut transaction).len();

    let removed = RecordId::from("r000100");
    transaction.delete(at("r000100"));

    let found = walked(&mut transaction);
    assert_eq!(found.len(), before - 1);
    assert!(!found.iter().any(|(id, _)| id == &removed));
    assert_eq!(found, scanned(&transaction));
}

#[test]
fn a_record_rewritten_in_this_transaction_is_handed_over_at_its_new_value() {
    let store = populated();
    let mut transaction = store.begin().unwrap();
    transaction.put(at("r000100"), payload("pending"));

    let found = walked(&mut transaction);
    let held = found
        .iter()
        .find(|(id, _)| id == &RecordId::from("r000100"))
        .expect("the rewritten record is still in the answer");
    assert_eq!(held.1, payload("pending"));
    assert_eq!(found, scanned(&transaction));
}
