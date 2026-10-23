//! `UPDATE t SET … WHERE …` — one statement that changes every record a
//! condition holds for (ADR-0134 D1).
//!
//! The record form `UPDATE t:1 … WHERE` is a compare-and-set and refuses when its
//! condition is false. The set form is a filter: a record the condition does
//! not hold for is left alone, and a set nothing matches answers `0`. The cases
//! below pin both halves, and that every rule a single update obeys — events,
//! assertions, indexes, the whole transaction — holds for many.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Six readings, four stale, with an index on `value` so a condition over it is
/// served by the index the edit then moves.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION readings;\n\
             DEFINE INDEX by_value ON readings FIELDS value;\n\
             CREATE readings:1 = { value: 1, stale: true };\n\
             CREATE readings:2 = { value: 2, stale: false };\n\
             CREATE readings:3 = { value: 3, stale: true };\n\
             CREATE readings:4 = { value: 4, stale: false };\n\
             CREATE readings:5 = { value: 5, stale: true };\n\
             CREATE readings:6 = { value: 6, stale: true };",
        )
        .unwrap();
    session
}

fn updated(outcome: &Outcome) -> i64 {
    match outcome {
        Outcome::Value(Value::Number(Number::Integer(count))) => *count,
        other => panic!("expected the count updated, got {other:?}"),
    }
}

fn last(session: &mut Session<'_>, statement: &str) -> Outcome {
    session
        .run_now(statement)
        .unwrap()
        .pop()
        .expect("an outcome")
}

/// Each record's id and the integer at `field`, in id order.
fn column(session: &mut Session<'_>, field: &str) -> Vec<(String, Option<i64>)> {
    let outcome = last(session, "SELECT * FROM readings;");
    outcome
        .records()
        .expect("records")
        .iter()
        .map(|(id, record)| {
            let held = match record {
                Value::Object(fields) => match fields.get(field) {
                    Some(Value::Number(Number::Integer(n))) => Some(*n),
                    Some(Value::Bool(true)) => Some(1),
                    Some(Value::Bool(false)) => Some(0),
                    _ => None,
                },
                _ => None,
            };
            (id.to_string(), held)
        })
        .collect()
}

#[test]
fn every_match_is_changed_and_the_answer_is_how_many() {
    let store = store();
    let mut session = ready(&store);

    let outcome = last(
        &mut session,
        "UPDATE readings SET checked = true WHERE stale = true;",
    );

    assert_eq!(updated(&outcome), 4);
    let checked: Vec<_> = column(&mut session, "checked")
        .into_iter()
        .map(|(id, held)| (id, held.is_some()))
        .collect();
    assert_eq!(
        checked,
        [
            ("1".to_owned(), true),
            ("2".to_owned(), false),
            ("3".to_owned(), true),
            ("4".to_owned(), false),
            ("5".to_owned(), true),
            ("6".to_owned(), true),
        ]
    );
}

#[test]
fn a_set_nothing_matches_answers_zero_and_is_not_an_error() {
    let store = store();
    let mut session = ready(&store);

    let outcome = last(
        &mut session,
        "UPDATE readings SET value = 0 WHERE value > 100;",
    );

    assert_eq!(updated(&outcome), 0);
    assert_eq!(
        column(&mut session, "value")
            .into_iter()
            .map(|(_, held)| held)
            .collect::<Vec<_>>(),
        [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]
    );
}

#[test]
fn the_bound_counts_records_changed_not_records_examined() {
    let store = store();
    let mut session = ready(&store);

    let outcome = last(
        &mut session,
        "UPDATE readings SET value = 0 WHERE stale = true LIMIT 2;",
    );

    assert_eq!(updated(&outcome), 2);
    let zeroed = column(&mut session, "value")
        .into_iter()
        .filter(|(_, held)| *held == Some(0))
        .count();
    assert_eq!(zeroed, 2);

    let all = last(
        &mut session,
        "UPDATE readings SET value = 0 WHERE stale = true LIMIT ALL;",
    );
    assert_eq!(updated(&all), 4);
}

#[test]
fn an_edit_that_moves_the_indexed_field_changes_each_record_once() {
    let store = store();
    let mut session = ready(&store);

    // Served by `by_value`, and every edit moves the record further along that
    // index — a walk that met its own writes would change a record twice.
    let outcome = last(
        &mut session,
        "UPDATE readings SET value = value + 10 WHERE value >= 1;",
    );

    assert_eq!(updated(&outcome), 6);
    assert_eq!(
        column(&mut session, "value")
            .into_iter()
            .map(|(_, held)| held)
            .collect::<Vec<_>>(),
        [Some(11), Some(12), Some(13), Some(14), Some(15), Some(16)]
    );
    let found = last(&mut session, "SELECT * FROM readings WHERE value > 10;");
    assert_eq!(found.records().expect("records").len(), 6);
}

#[test]
fn merge_folds_the_object_into_every_match() {
    let store = store();
    let mut session = ready(&store);

    let outcome = last(
        &mut session,
        "UPDATE readings MERGE { reviewed: 1 } WHERE stale = false;",
    );

    assert_eq!(updated(&outcome), 2);
    assert_eq!(
        column(&mut session, "reviewed")
            .into_iter()
            .map(|(_, held)| held)
            .collect::<Vec<_>>(),
        [None, Some(1), None, Some(1), None, None]
    );
}

#[test]
fn return_after_answers_the_records_as_they_became() {
    let store = store();
    let mut session = ready(&store);

    let outcome = last(
        &mut session,
        "UPDATE readings SET value = value * 2 WHERE stale = false RETURN AFTER;",
    );

    let answered: Vec<_> = outcome
        .records()
        .expect("records")
        .iter()
        .map(|(id, record)| match record {
            Value::Object(fields) => (id.to_string(), fields.get("value").cloned()),
            other => panic!("a record, got {other:?}"),
        })
        .collect();
    assert_eq!(
        answered,
        [
            ("2".to_owned(), Some(Value::Number(Number::Integer(4)))),
            ("4".to_owned(), Some(Value::Number(Number::Integer(8)))),
        ]
    );
}

#[test]
fn return_before_answers_the_records_as_they_stood() {
    let store = store();
    let mut session = ready(&store);

    let outcome = last(
        &mut session,
        "UPDATE readings SET value = 0 WHERE stale = false RETURN BEFORE;",
    );

    let answered: Vec<_> = outcome
        .records()
        .expect("records")
        .iter()
        .map(|(id, record)| match record {
            Value::Object(fields) => (id.to_string(), fields.get("value").cloned()),
            other => panic!("a record, got {other:?}"),
        })
        .collect();
    assert_eq!(
        answered,
        [
            ("2".to_owned(), Some(Value::Number(Number::Integer(2)))),
            ("4".to_owned(), Some(Value::Number(Number::Integer(4)))),
        ]
    );
}

#[test]
fn the_set_form_needs_its_condition() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("UPDATE readings SET value = 0;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("`WHERE`"), "{refused}");
    assert_eq!(
        column(&mut session, "value")
            .into_iter()
            .filter(|(_, held)| *held == Some(0))
            .count(),
        0
    );
}

#[test]
fn the_set_form_refuses_a_whole_record_replace() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("UPDATE readings = { value: 0 } WHERE stale = true;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("`SET` or `MERGE`"), "{refused}");
}

#[test]
fn the_record_form_keeps_its_compare_and_set() {
    let store = store();
    let mut session = ready(&store);

    // readings:2 is not stale, so the record form refuses rather than answering
    // that it changed nothing.
    let refused = session
        .run_now("UPDATE readings:2 SET value = 0 WHERE stale = true;")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("does not say what the condition asserts"),
        "{refused}"
    );
}

#[test]
fn an_event_runs_once_for_each_record_changed() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE COLLECTION audit;\n\
             DEFINE EVENT noted ON readings FOR UPDATE THEN CREATE audit = { reading: $id };",
        )
        .unwrap();

    let outcome = last(
        &mut session,
        "UPDATE readings SET seen = 1 WHERE stale = true;",
    );

    assert_eq!(updated(&outcome), 4);
    let audited = last(&mut session, "SELECT * FROM audit;");
    assert_eq!(audited.records().expect("records").len(), 4);
}

#[test]
fn a_refusal_on_any_record_leaves_every_record_as_it_was() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE readings SCHEMALESS;\n\
             DEFINE FIELD value ON readings TYPE int ASSERT $value < 6;\n\
             CREATE readings:1 = { value: 1 };\n\
             CREATE readings:2 = { value: 5 };\n\
             CREATE readings:3 = { value: 3 };",
        )
        .unwrap();

    // readings:2 would become 6, which its field refuses — and the two records
    // changed before it must not stay changed.
    let refused = session
        .run_now("UPDATE readings SET value = value + 1 WHERE value > 0;")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("record 2 of table readings holds a value its declaration refuses"),
        "{refused}"
    );

    assert_eq!(
        column(&mut session, "value")
            .into_iter()
            .map(|(_, held)| held)
            .collect::<Vec<_>>(),
        [Some(1), Some(5), Some(3)]
    );
}

#[test]
fn a_vault_is_changed_one_record_at_a_time() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "UNSEAL VAULT WITH 'an operator passphrase';\n\
             DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE VAULT secrets;\n\
             DEFINE FIELD token ON secrets TYPE string SECRET;\n\
             CREATE secrets:1 = { token: 'a' };",
        )
        .unwrap();

    let refused = session
        .run_now("UPDATE secrets SET token = 'b' WHERE token = 'a';")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("secrets is a vault"), "{refused}");
}
