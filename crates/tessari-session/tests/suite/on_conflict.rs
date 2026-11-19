//! `INSERT … ON CONFLICT`: the arbiter branch — a unique collision takes the
//! alternative action atomically instead of refusing the statement.
//!
//! The shape the tests pin: the conflict target names columns, the arbiter is
//! the unique index holding exactly those columns (the order-insensitive
//! inference rule), `excluded` is the proposed row after defaults, a false
//! WHERE skips the row rather than refusing it, a violation on a different
//! unique index still refuses, and one statement may not touch one row twice.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Now, RunNow};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION users;
DEFINE INDEX by_email ON users FIELDS email UNIQUE;
",
        )
        .unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run_now(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

fn everyone(session: &mut Session<'_>) -> Vec<(RecordId, Value)> {
    run(session, "SELECT * FROM users;")
        .records()
        .expect("records")
        .to_vec()
}

fn field(record: &Value, name: &str) -> Value {
    let Value::Object(fields) = record else {
        panic!("not an object")
    };
    fields.get(name).expect("the field").clone()
}

fn refused(session: &mut Session<'_>, script: &str) -> Error {
    session.run_now(script).expect_err("the statement refuses")
}

#[test]
fn do_nothing_skips_the_conflicting_row_and_lands_the_rest() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    let outcome = run(
        &mut session,
        "INSERT INTO users (name, email) VALUES \
         ('grace', 'grace@x'), ('ada again', 'ada@x'), ('alan', 'alan@x') \
         ON CONFLICT (email) DO NOTHING;",
    );
    // Only the two landed rows answer; the conflicting row was skipped, not
    // counted.
    assert_eq!(outcome.keys().expect("keys").len(), 2, "{outcome:?}");

    let held = everyone(&mut session);
    assert_eq!(held.len(), 3, "{held:?}");
    assert!(
        held.iter()
            .any(|(_, record)| field(record, "email") == Value::String("grace@x".to_owned())),
        "the sibling before the conflict landed: {held:?}"
    );
    assert!(
        held.iter()
            .any(|(_, record)| field(record, "email") == Value::String("alan@x".to_owned())),
        "the sibling after the conflict landed: {held:?}"
    );
    assert!(
        held.iter()
            .any(|(_, record)| field(record, "name") == Value::String("ada".to_owned())),
        "the existing record kept its own values: {held:?}"
    );
}

#[test]
fn do_update_merges_the_proposed_row_with_excluded() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    let outcome = run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada lovelace', 'ada@x') \
         ON CONFLICT (email) DO UPDATE SET name = excluded.name;",
    );
    // The answer addresses the existing record: a caller can follow it.
    let keys = outcome.keys().expect("keys");
    assert_eq!(keys.len(), 1, "{outcome:?}");

    let held = everyone(&mut session);
    assert_eq!(held.len(), 1, "no second record was written: {held:?}");
    assert_eq!(
        field(&held[0].1, "name"),
        Value::String("ada lovelace".to_owned()),
        "the update landed: {held:?}"
    );
    assert_eq!(keys[0], held[0].0, "the key addresses the updated record");
}

#[test]
fn the_set_clause_sees_the_record_as_it_was_and_swaps() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ignored', 'ada@x') \
         ON CONFLICT (email) DO UPDATE SET name = email, email = name;",
    );
    // `SET a = b, b = a` swaps: every right-hand side reads the record as it
    // stood, and `name` and `email` exchange places.
    let held = everyone(&mut session);
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        field(&held[0].1, "name"),
        Value::String("ada@x".to_owned()),
        "{held:?}"
    );
    assert_eq!(
        field(&held[0].1, "email"),
        Value::String("ada".to_owned()),
        "{held:?}"
    );
}

#[test]
fn a_false_where_skips_the_update_and_refuses_nothing() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    let outcome = run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada lovelace', 'ada@x') \
         ON CONFLICT (email) DO UPDATE SET name = excluded.name \
         WHERE name = 'somebody else';",
    );
    assert_eq!(outcome.keys().expect("keys").len(), 0, "{outcome:?}");
    let held = everyone(&mut session);
    assert_eq!(
        field(&held[0].1, "name"),
        Value::String("ada".to_owned()),
        "a false WHERE leaves the record alone: {held:?}"
    );
}

#[test]
fn a_true_where_applies_the_update() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada lovelace', 'ada@x') \
         ON CONFLICT (email) DO UPDATE SET name = excluded.name \
         WHERE name = 'ada';",
    );
    let held = everyone(&mut session);
    assert_eq!(
        field(&held[0].1, "name"),
        Value::String("ada lovelace".to_owned()),
        "{held:?}"
    );
}

#[test]
fn a_violation_on_a_non_arbiter_index_still_refuses() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE INDEX by_name ON users FIELDS name UNIQUE;")
        .unwrap();
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    let error = refused(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'other@x') \
         ON CONFLICT (email) DO NOTHING;",
    );
    assert!(
        matches!(
            error,
            Error::Store(tessari_storage::Error::UniqueViolation { .. })
        ),
        "the name collision is not the email arbiter's business: {error:?}"
    );
    assert_eq!(everyone(&mut session).len(), 1, "nothing of it landed");
}

#[test]
fn a_missing_arbiter_refuses_conflict_target_not_unique() {
    let store = store();
    let mut session = ready(&store);
    let error = refused(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x') \
         ON CONFLICT (name) DO NOTHING;",
    );
    assert!(
        matches!(error, Error::ConflictTargetNotUnique { .. }),
        "no unique index holds exactly (name): {error:?}"
    );
}

#[test]
fn the_query_form_refuses_by_name() {
    let store = store();
    let mut session = ready(&store);
    let error = refused(
        &mut session,
        "INSERT INTO users (name, email) SELECT name, email FROM users \
         ON CONFLICT (email) DO NOTHING;",
    );
    assert!(
        matches!(error, Error::OnConflictOnQuery { .. }),
        "the query form is its own future: {error:?}"
    );
}

#[test]
fn one_statement_may_not_touch_one_row_twice() {
    let store = store();
    let mut session = ready(&store);
    let error = refused(
        &mut session,
        "INSERT INTO users (name, email) VALUES \
         ('first', 'same@x'), ('second', 'same@x') \
         ON CONFLICT (email) DO NOTHING;",
    );
    // The same-batch duplicate is the cardinality refusal, not a skip: the
    // row the statement already proposed is the conflict.
    assert!(
        matches!(
            error,
            Error::Store(tessari_storage::Error::UniqueViolation { .. })
        ),
        "a row may not answer twice in one statement: {error:?}"
    );
}

#[test]
fn return_after_answers_the_outcome_rows() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('ada', 'ada@x');",
    );

    let outcome = run(
        &mut session,
        "INSERT INTO users (name, email) VALUES \
         ('grace', 'grace@x'), ('ada lovelace', 'ada@x') \
         ON CONFLICT (email) DO UPDATE SET name = excluded.name RETURN AFTER;",
    );
    let records = outcome.records().expect("the outcome rows");
    assert_eq!(records.len(), 2, "{outcome:?}");
    let names = records
        .iter()
        .map(|(_, record)| field(record, "name"))
        .collect::<Vec<_>>();
    assert!(
        names.contains(&Value::String("grace".to_owned())),
        "the inserted row answers: {names:?}"
    );
    assert!(
        names.contains(&Value::String("ada lovelace".to_owned())),
        "the updated row answers: {names:?}"
    );
}
