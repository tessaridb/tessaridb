//! What a conditional delete's bound counts, and that the bound is optional.
//!
//! `DELETE FROM t WHERE …` takes an optional bound (ADR-0134 D1, owner
//! 2026-10-23): with none it removes every record the condition holds for,
//! `LIMIT n` removes at most that many, and `LIMIT ALL` says the first on the
//! statement. The condition itself stays required.
//!
//! The property that needed pinning is **what the bound counts**. It counts
//! records *removed*, not records *examined*. A bound applied to candidates
//! would let the same statement over the same data remove a different set
//! depending on which index answered it, and the index is not something the
//! author of the statement chose.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Six readings, four of which are stale. The two live ones exist so a bound
/// that counted candidates instead of survivors would be visible: an index or a
/// scan walks all six, and only four are ever eligible.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION readings;\n\
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

fn removed(outcome: &Outcome) -> u64 {
    match outcome {
        Outcome::Removed { count } => *count,
        other => panic!("expected a removal count, got {other:?}"),
    }
}

fn rows(session: &mut Session<'_>, statement: &str) -> usize {
    let outcome = session.run_now(statement).unwrap();
    let last = outcome.last().expect("an outcome");
    last.records().expect("records").len()
}

#[test]
fn a_conditional_delete_with_no_bound_removes_every_match() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 4);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 2);
}

#[test]
fn a_conditional_delete_still_needs_its_condition() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("DELETE FROM readings;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("`WHERE`"), "{refused}");
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 6);
}

#[test]
fn a_span_delete_with_no_bound_removes_the_span() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session.run_now("DELETE FROM readings:2..=4;").unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 3);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 3);
}

#[test]
fn a_bound_removes_at_most_what_it_names() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT 2;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 2);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 4);
}

#[test]
fn the_bound_counts_survivors_and_not_candidates() {
    let store = store();
    let mut session = ready(&store);

    // Four records satisfy the condition and six exist. A bound of four removes
    // all four; a bound that counted the walk would stop at the fourth record
    // *examined* and leave at least one stale record behind.
    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT 4;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 4);
    assert_eq!(
        rows(&mut session, "SELECT * FROM readings WHERE stale = true;"),
        0,
        "every stale record was within the bound, so none may remain"
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM readings;"),
        2,
        "and the records the condition never held for are untouched"
    );
}

#[test]
fn limit_all_removes_every_record_the_condition_holds_for() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT ALL;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 4);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 2);
}

#[test]
fn a_bound_larger_than_the_match_removes_the_match() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT 1000;")
        .unwrap();

    assert_eq!(
        removed(outcome.last().expect("an outcome")),
        4,
        "a bound is a ceiling, not a quota"
    );
}

#[test]
fn a_bound_of_zero_removes_nothing_and_is_not_an_error() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT 0;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 0);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 6);
}

#[test]
fn the_single_record_form_needs_no_bound() {
    let store = store();
    let mut session = ready(&store);

    // It names one record, so the question the clause answers does not arise.
    session.run_now("DELETE readings:1;").unwrap();

    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 5);
}

#[test]
fn a_condition_nothing_satisfies_removes_nothing_and_is_not_an_error() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run_now("DELETE FROM readings WHERE value > 100;")
        .unwrap();
    assert_eq!(removed(outcome.last().expect("an outcome")), 0);

    let outcome = session
        .run_now("DELETE FROM readings WHERE value > 100 LIMIT ALL;")
        .unwrap();
    assert_eq!(removed(outcome.last().expect("an outcome")), 0);
}

/// The ids a statement answered with and each record's `value`, in order.
fn returned(outcome: &Outcome) -> Vec<(String, i64)> {
    let records = outcome.records().expect("records");
    records
        .iter()
        .map(|(id, record)| {
            let tessari_types::Value::Object(fields) = record else {
                panic!("a record answered {record:?}");
            };
            let Some(tessari_types::Value::Number(tessari_types::Number::Integer(value))) =
                fields.get("value")
            else {
                panic!("no value in {fields:?}");
            };
            (id.to_string(), *value)
        })
        .collect()
}

/// `RETURN BEFORE` on a delete of many answers what it removed, as each record
/// stood, so a caller acting on the removed set needs no read before it.
#[test]
fn a_conditional_delete_returning_before_answers_the_records_it_removed() {
    let store = store();
    let mut session = ready(&store);
    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT 2 RETURN BEFORE;")
        .unwrap();
    assert_eq!(
        returned(outcome.last().expect("an outcome")),
        vec![("1".to_owned(), 1), ("3".to_owned(), 3)]
    );
    let outcome = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT ALL RETURN BEFORE;")
        .unwrap();
    assert_eq!(
        returned(outcome.last().expect("an outcome")),
        vec![("5".to_owned(), 5), ("6".to_owned(), 6)]
    );
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 2);
}

#[test]
fn a_span_delete_returning_before_answers_the_records_it_removed() {
    let store = store();
    let mut session = ready(&store);
    let outcome = session
        .run_now("DELETE FROM readings:1..=2 LIMIT ALL RETURN BEFORE;")
        .unwrap();
    assert_eq!(
        returned(outcome.last().expect("an outcome")),
        vec![("1".to_owned(), 1), ("2".to_owned(), 2)]
    );
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 4);
}

/// A delete has no record after it, so `RETURN AFTER` could only answer
/// nothing — refused naming the word that works, as the single-record form is.
#[test]
fn a_delete_of_many_refuses_return_after() {
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run_now("DELETE FROM readings WHERE stale = true LIMIT ALL RETURN AFTER;")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("`BEFORE`"), "{refused}");
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 6);
}
