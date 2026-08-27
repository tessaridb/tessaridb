//! A conditional delete has to say how much it may remove.
//!
//! `DELETE FROM t WHERE …` used to accept no bound at all, which made emptying a
//! table one mistyped character away from a retention policy. The clause is now
//! required: `LIMIT n` removes at most that many, `LIMIT ALL` removes every
//! record the condition holds for and says so on the statement.
//!
//! The property that needed pinning is not that the bound exists — it is **what
//! the bound counts**. It counts records *removed*, not records *examined*. A
//! bound applied to candidates would let the same statement over the same data
//! remove a different set depending on which index answered it, and the index is
//! not something the author of the statement chose.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Six readings, four of which are stale. The two live ones exist so a bound
/// that counted candidates instead of survivors would be visible: an index or a
/// scan walks all six, and only four are ever eligible.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE readings;\n\
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
    let outcome = session.run(statement).unwrap();
    let last = outcome.last().expect("an outcome");
    last.records().expect("records").len()
}

#[test]
fn an_unbounded_conditional_delete_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let refused = session.run("DELETE FROM readings WHERE stale = true;");

    assert!(
        refused.is_err(),
        "an unbounded conditional delete must not run: {refused:?}"
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM readings;"),
        6,
        "a refused statement removes nothing"
    );
}

#[test]
fn a_bound_removes_at_most_what_it_names() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run("DELETE FROM readings WHERE stale = true LIMIT 2;")
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
        .run("DELETE FROM readings WHERE stale = true LIMIT 4;")
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
        .run("DELETE FROM readings WHERE stale = true LIMIT ALL;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 4);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 2);
}

#[test]
fn a_bound_larger_than_the_match_removes_the_match() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session
        .run("DELETE FROM readings WHERE stale = true LIMIT 1000;")
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
        .run("DELETE FROM readings WHERE stale = true LIMIT 0;")
        .unwrap();

    assert_eq!(removed(outcome.last().expect("an outcome")), 0);
    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 6);
}

#[test]
fn the_single_record_form_needs_no_bound() {
    let store = store();
    let mut session = ready(&store);

    // It names one record, so the question the clause answers does not arise.
    session.run("DELETE readings:1;").unwrap();

    assert_eq!(rows(&mut session, "SELECT * FROM readings;"), 5);
}

#[test]
fn a_condition_nothing_satisfies_still_needs_its_bound() {
    let store = store();
    let mut session = ready(&store);

    // The requirement is a property of the statement, not of the data — a
    // grammar that let a delete through because it happened to match nothing
    // would be checking after the fact.
    assert!(
        session
            .run("DELETE FROM readings WHERE value > 100;")
            .is_err()
    );

    let outcome = session
        .run("DELETE FROM readings WHERE value > 100 LIMIT ALL;")
        .unwrap();
    assert_eq!(removed(outcome.last().expect("an outcome")), 0);
}
