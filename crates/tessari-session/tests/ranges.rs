//! An ordered comparison served by the index that already sorts.
//!
//! The claim is that adding an index to a table changes only what a range read
//! costs. That is easy to assert loosely and easy to get wrong at exactly one
//! place: the **bounds**. The index encoding normalises — `1`, `1.0` and
//! `dec 1.00` become one byte string — so an exclusive end cannot be expressed
//! in bytes, and the scan takes both ends inclusive and lets the condition
//! discard what it over-fetched.
//!
//! Every fixture below therefore holds values sitting exactly on each bound,
//! because that is the only place the two paths can disagree.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;",
        )
        .unwrap();
    session
}

/// Ten readings, so bounds of 3 and 7 sit exactly on stored values.
const READINGS: &str = "DEFINE COLLECTION readings;\n\
     CREATE readings:1 = { level: 1, name: 'a', at: datetime '2026-01-01T00:00:00Z' };\n\
     CREATE readings:2 = { level: 2, name: 'b', at: datetime '2026-01-02T00:00:00Z' };\n\
     CREATE readings:3 = { level: 3, name: 'c', at: datetime '2026-01-03T00:00:00Z' };\n\
     CREATE readings:4 = { level: 4, name: 'd', at: datetime '2026-01-04T00:00:00Z' };\n\
     CREATE readings:5 = { level: 5, name: 'e', at: datetime '2026-01-05T00:00:00Z' };\n\
     CREATE readings:6 = { level: 6, name: 'f', at: datetime '2026-01-06T00:00:00Z' };\n\
     CREATE readings:7 = { level: 7, name: 'g', at: datetime '2026-01-07T00:00:00Z' };\n\
     CREATE readings:8 = { level: 8, name: 'h', at: datetime '2026-01-08T00:00:00Z' };\n\
     CREATE readings:9 = { level: 9, name: 'i', at: datetime '2026-01-09T00:00:00Z' };\n\
     CREATE readings:10 = { level: 10, name: 'j', at: datetime '2026-01-10T00:00:00Z' };";

/// The same condition with and without indexes, as ids and access path.
fn both_ways(condition: &str, indexes: &str) -> (Vec<RecordId>, Vec<RecordId>, AccessPath) {
    let answer = |declare: bool| {
        let store = store();
        let mut session = ready(&store);
        session.run(READINGS).unwrap();
        if declare {
            session.run(indexes).unwrap();
        }
        let outcomes = session
            .run(&format!("SELECT * FROM readings WHERE {condition};"))
            .unwrap();
        let path = outcomes[0].path().unwrap();
        let ids: Vec<RecordId> = outcomes[0]
            .records()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        (ids, path)
    };
    let (scanned, scan_path) = answer(false);
    assert_eq!(
        scan_path,
        AccessPath::Scan,
        "the reference read used an index"
    );
    let (served, path) = answer(true);
    (scanned, served, path)
}

const BY_LEVEL: &str = "DEFINE INDEX by_level ON readings FIELDS level;";

fn ints(held: &[i64]) -> Vec<RecordId> {
    held.iter().map(|n| RecordId::Int(*n)).collect()
}

#[test]
fn each_of_the_four_orderings_is_served_and_answers_what_the_scan_did() {
    // The bounds are 3 and 7, which are stored values — so an inclusive end that
    // should have been exclusive shows up as one extra row rather than as
    // nothing at all.
    for (condition, expected) in [
        ("level > 7", ints(&[8, 9, 10])),
        ("level >= 7", ints(&[7, 8, 9, 10])),
        ("level < 3", ints(&[1, 2])),
        ("level <= 3", ints(&[1, 2, 3])),
    ] {
        let (scanned, served, path) = both_ways(condition, BY_LEVEL);
        assert_eq!(path, AccessPath::Index, "not served: {condition}");
        assert_eq!(scanned, expected, "the scan disagrees: {condition}");
        assert_eq!(served, expected, "the index disagrees: {condition}");
    }
}

#[test]
fn two_bounds_on_one_path_become_one_scan_in_either_written_order() {
    // Serving only one of them would read half a table to find a day.
    for condition in ["level >= 3 AND level < 7", "level < 7 AND level >= 3"] {
        let (scanned, served, path) = both_ways(condition, BY_LEVEL);
        assert_eq!(path, AccessPath::Index, "not served: {condition}");
        assert_eq!(scanned, ints(&[3, 4, 5, 6]));
        assert_eq!(served, ints(&[3, 4, 5, 6]));
    }
}

#[test]
fn the_tighter_of_two_bounds_in_one_direction_wins() {
    let (scanned, served, path) = both_ways("level > 2 AND level > 6", BY_LEVEL);
    assert_eq!(path, AccessPath::Index);
    assert_eq!(scanned, ints(&[7, 8, 9, 10]));
    assert_eq!(served, ints(&[7, 8, 9, 10]));
}

#[test]
fn an_empty_range_answers_with_nothing_rather_than_everything() {
    // A bound pair that crosses is the case where a wrong scan bound produces
    // the whole table instead of none, and nothing else would notice.
    let (scanned, served, path) = both_ways("level > 8 AND level < 3", BY_LEVEL);
    assert_eq!(path, AccessPath::Index);
    assert!(scanned.is_empty());
    assert!(served.is_empty());
}

#[test]
fn a_range_past_either_end_answers_the_same_either_way() {
    for (condition, expected) in [
        ("level > 100", Vec::new()),
        ("level < 0", Vec::new()),
        ("level >= 1", ints(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])),
    ] {
        let (scanned, served, path) = both_ways(condition, BY_LEVEL);
        assert_eq!(path, AccessPath::Index, "not served: {condition}");
        assert_eq!(scanned, expected, "{condition}");
        assert_eq!(served, expected, "{condition}");
    }
}

#[test]
fn a_range_is_not_a_time_feature() {
    // Built as an ordered read over the value system rather than as a
    // time-series clause, so the same statement works for whatever the field
    // holds. If it only worked for datetimes it would be a narrower thing with a
    // date in its name.
    let (scanned, served, path) = both_ways(
        "at >= datetime '2026-01-03T00:00:00Z' AND at < datetime '2026-01-06T00:00:00Z'",
        "DEFINE INDEX by_at ON readings FIELDS at;",
    );
    assert_eq!(path, AccessPath::Index);
    assert_eq!(scanned, ints(&[3, 4, 5]));
    assert_eq!(served, ints(&[3, 4, 5]));

    let (scanned, served, path) = both_ways(
        "name > 'c' AND name <= 'f'",
        "DEFINE INDEX by_name ON readings FIELDS name;",
    );
    assert_eq!(path, AccessPath::Index);
    assert_eq!(scanned, ints(&[4, 5, 6]));
    assert_eq!(served, ints(&[4, 5, 6]));
}

#[test]
fn a_range_over_numbers_of_different_kinds_agrees_with_the_scan() {
    // The index encoding normalises `7`, `7.0` and `dec 7.00` to one byte
    // string, so a bound written as one kind must find records holding another.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION mixed;\n\
             DEFINE INDEX by_size ON mixed FIELDS size;\n\
             CREATE mixed:1 = { size: 7 };\n\
             CREATE mixed:2 = { size: 7.0 };\n\
             CREATE mixed:3 = { size: dec 7.00 };\n\
             CREATE mixed:4 = { size: 8 };",
        )
        .unwrap();

    let outcomes = session
        .run("SELECT * FROM mixed WHERE size >= 7 AND size < 8;")
        .unwrap();
    assert_eq!(outcomes[0].path(), Some(AccessPath::Index));
    assert_eq!(outcomes[0].records().unwrap().len(), 3);
}

#[test]
fn a_bound_that_reads_the_record_is_not_a_bound() {
    // `level > other` compares two fields, so there is no constant to seek to —
    // and a range whose end is only known per record is not a range at all.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION pairs;\n\
             DEFINE INDEX by_left ON pairs FIELDS left;\n\
             CREATE pairs:1 = { left: 5, right: 3 };\n\
             CREATE pairs:2 = { left: 1, right: 9 };",
        )
        .unwrap();

    let outcomes = session
        .run("SELECT * FROM pairs WHERE left > right;")
        .unwrap();
    assert_eq!(outcomes[0].path(), Some(AccessPath::Scan));
    assert_eq!(outcomes[0].records().unwrap().len(), 1);
}

#[test]
fn a_path_with_no_index_is_still_a_scan() {
    let (scanned, served, path) =
        both_ways("level > 7", "DEFINE INDEX by_name ON readings FIELDS name;");
    assert_eq!(path, AccessPath::Scan);
    assert_eq!(scanned, served);
}

#[test]
fn a_record_written_and_not_yet_committed_is_in_the_range() {
    // Every other index read folds this transaction's own writes in, because a
    // writer that cannot find what it just wrote is a store with two answers.
    let store = store();
    let mut session = ready(&store);
    session.run(READINGS).unwrap();
    session.run(BY_LEVEL).unwrap();

    let outcomes = session
        .run(
            "BEGIN;\n\
             CREATE readings:11 = { level: 11, name: 'k' };\n\
             SELECT * FROM readings WHERE level > 9;\n\
             COMMIT;",
        )
        .unwrap();
    let found: Vec<RecordId> = outcomes[2]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(found, ints(&[10, 11]));
}

#[test]
fn a_range_and_an_equality_in_one_condition_let_the_planner_choose() {
    // The equality has a known ceiling on a unique index; the range has none. So
    // the plan takes the equality and the range narrows nothing — which is a
    // decision about cost and, as ever, not about the answer.
    let store = store();
    let mut session = ready(&store);
    session.run(READINGS).unwrap();
    session
        .run(
            "DEFINE INDEX by_level ON readings FIELDS level;\n\
             DEFINE INDEX by_name ON readings FIELDS name UNIQUE;",
        )
        .unwrap();

    let outcomes = session
        .run("SELECT * FROM readings WHERE level > 1 AND name = 'e';")
        .unwrap();
    assert_eq!(outcomes[0].path(), Some(AccessPath::Index));
    let found: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(found, ints(&[5]));
}
