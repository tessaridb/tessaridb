//! `RETURN BEFORE` and `RETURN AFTER` — a write answering with what it wrote.
//!
//! Every write used to be followed by a read: a second statement, and over the
//! wire a second round trip, to learn a value the store had in hand a moment
//! earlier. The clause removes the second statement rather than adding a
//! feature.
//!
//! It is **absent by default**, and that is the load-bearing half of the design:
//! a store that shipped the changed record back on every write would make the
//! common case pay for the rare one.
//!
//! The two refusals below are the other half. `CREATE … RETURN BEFORE` and
//! `DELETE … RETURN AFTER` could only ever answer `NONE`, and answering `NONE`
//! to a question somebody plainly meant is the silent-wrong-answer shape this
//! language spends its rules removing.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'ada', visits: 3 };",
        )
        .unwrap();
    session
}

/// The single value a statement answered with.
fn answered(session: &mut Session<'_>, statement: &str) -> Value {
    let outcome = session.run(statement).unwrap();
    match outcome.last().expect("an outcome") {
        Outcome::Value(value) => value.clone(),
        other => panic!("expected a value, got {other:?}"),
    }
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(fields) = value else {
        panic!("not an object: {value:?}");
    };
    fields
        .get(name)
        .unwrap_or_else(|| panic!("no {name} in {fields:?}"))
}

fn text(value: &Value) -> &str {
    match value {
        Value::String(held) => held,
        other => panic!("not a string: {other:?}"),
    }
}

#[test]
fn a_write_without_the_clause_still_answers_done() {
    let store = store();
    let mut session = ready(&store);

    let outcome = session.run("UPDATE users:1 SET visits = 4;").unwrap();

    assert_eq!(
        outcome.last().expect("an outcome"),
        &Outcome::Done,
        "the clause is opt-in; the default must not change"
    );
}

#[test]
fn update_after_answers_the_record_as_it_now_stands() {
    let store = store();
    let mut session = ready(&store);

    let held = answered(&mut session, "UPDATE users:1 SET visits = 4 RETURN AFTER;");

    assert_eq!(field(&held, "visits"), &Value::Number(Number::Integer(4)));
    assert_eq!(
        text(field(&held, "name")),
        "ada",
        "the whole record, not just what changed"
    );
}

#[test]
fn update_before_answers_the_record_as_it_stood() {
    let store = store();
    let mut session = ready(&store);

    let held = answered(&mut session, "UPDATE users:1 SET visits = 4 RETURN BEFORE;");

    assert_eq!(
        field(&held, "visits"),
        &Value::Number(Number::Integer(3)),
        "what was overwritten is what a caller cannot read afterwards"
    );
}

#[test]
fn the_write_still_happened_when_it_answered_with_before() {
    let store = store();
    let mut session = ready(&store);

    answered(&mut session, "UPDATE users:1 SET visits = 4 RETURN BEFORE;");

    let now = answered(&mut session, "UPSERT users:1 SET visits = 5 RETURN BEFORE;");
    assert_eq!(
        field(&now, "visits"),
        &Value::Number(Number::Integer(4)),
        "the first update landed even though it reported the older value"
    );
}

#[test]
fn create_after_answers_the_record_it_wrote() {
    let store = store();
    let mut session = ready(&store);

    let held = answered(
        &mut session,
        "CREATE users:2 = { name: 'grace' } RETURN AFTER;",
    );

    assert_eq!(text(field(&held, "name")), "grace");
}

#[test]
fn delete_before_answers_what_it_removed() {
    let store = store();
    let mut session = ready(&store);

    let held = answered(&mut session, "DELETE users:1 RETURN BEFORE;");

    assert_eq!(text(field(&held, "name")), "ada");
}

#[test]
fn upsert_before_answers_none_when_there_was_nothing() {
    let store = store();
    let mut session = ready(&store);

    // The true answer, and the one that distinguishes an upsert that created
    // from one that replaced — which is the reason to ask.
    let held = answered(
        &mut session,
        "UPSERT users:9 = { name: 'joan' } RETURN BEFORE;",
    );
    assert_eq!(held, Value::None);

    let again = answered(
        &mut session,
        "UPSERT users:9 = { name: 'joan' } RETURN BEFORE;",
    );
    assert_eq!(text(field(&again, "name")), "joan");
}

#[test]
fn merge_after_answers_the_folded_record() {
    let store = store();
    let mut session = ready(&store);

    let held = answered(
        &mut session,
        "UPDATE users:1 MERGE { city: 'Paris' } RETURN AFTER;",
    );

    assert_eq!(text(field(&held, "city")), "Paris");
    assert_eq!(
        text(field(&held, "name")),
        "ada",
        "the clause reports the record the fold produced, not the fold"
    );
}

#[test]
fn the_two_meaningless_pairings_are_refused() {
    let store = store();
    let mut session = ready(&store);

    assert!(
        session
            .run("CREATE users:3 = { name: 'x' } RETURN BEFORE;")
            .is_err(),
        "there is no record before a create"
    );
    assert!(
        session.run("DELETE users:1 RETURN AFTER;").is_err(),
        "there is no record after a delete"
    );
}

#[test]
fn a_refused_clause_writes_nothing() {
    let store = store();
    let mut session = ready(&store);

    // Refused when the statement is read, so the write never starts.
    let _ = session.run("CREATE users:3 = { name: 'x' } RETURN BEFORE;");

    let outcome = session.run("SELECT * FROM users;").unwrap();
    let records = outcome
        .last()
        .expect("an outcome")
        .records()
        .expect("records");
    assert_eq!(records.len(), 1);
}

#[test]
fn return_needs_one_of_the_two_words() {
    let store = store();
    let mut session = ready(&store);

    assert!(
        session
            .run("UPDATE users:1 SET visits = 4 RETURN;")
            .is_err()
    );
    assert!(
        session
            .run("UPDATE users:1 SET visits = 4 RETURN DIFF;")
            .is_err(),
        "DIFF is not built, and is refused rather than silently ignored"
    );
}
