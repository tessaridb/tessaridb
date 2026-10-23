//! A field declared with a `DEFAULT` gives it to the records already there
//! (ADR-0134 D2).
//!
//! Before this a default reached only later writes, so a table that gained a
//! field read back without it on every old record, and `REQUIRED DEFAULT` could
//! not be added to a table that held anything. The declaration now fills each
//! record whose field is absent, in the declaration's own transaction.

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

/// Three records: one without `plan`, one holding a plan, one holding `NULL`.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users SCHEMALESS;\n\
             CREATE users:1 = { name: 'ada' };\n\
             CREATE users:2 = { name: 'grace', plan: 'pro' };\n\
             CREATE users:3 = { name: 'alan', plan: NULL };",
        )
        .unwrap();
    session
}

fn last(session: &mut Session<'_>, statement: &str) -> Outcome {
    session
        .run_now(statement)
        .unwrap()
        .pop()
        .expect("an outcome")
}

/// Each record's id and what it holds at `field`, in id order.
fn column(session: &mut Session<'_>, field: &str) -> Vec<(String, Value)> {
    last(session, "SELECT * FROM users;")
        .records()
        .expect("records")
        .iter()
        .map(|(id, record)| match record {
            Value::Object(fields) => (
                id.to_string(),
                fields.get(field).cloned().unwrap_or(Value::None),
            ),
            other => panic!("a record, got {other:?}"),
        })
        .collect()
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

#[test]
fn adding_a_field_with_a_default_fills_the_records_without_it() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("ALTER TABLE users ADD FIELD plan TYPE string DEFAULT 'free';")
        .unwrap();

    // The absent one is filled; a value and an explicit `NULL` were written on
    // purpose and stay as they were.
    assert_eq!(
        column(&mut session, "plan"),
        [
            ("1".to_owned(), text("free")),
            ("2".to_owned(), text("pro")),
            ("3".to_owned(), Value::Null),
        ]
    );
}

#[test]
fn define_field_fills_them_too() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("DEFINE FIELD visits ON users TYPE int DEFAULT 0;")
        .unwrap();

    assert_eq!(
        column(&mut session, "visits")
            .into_iter()
            .map(|(_, held)| held)
            .collect::<Vec<_>>(),
        [
            Value::Number(Number::Integer(0)),
            Value::Number(Number::Integer(0)),
            Value::Number(Number::Integer(0)),
        ]
    );
}

#[test]
fn a_required_field_with_a_default_can_be_added_to_a_table_that_holds_records() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("ALTER TABLE users ADD FIELD visits TYPE int REQUIRED DEFAULT 0;")
        .unwrap();

    assert!(
        column(&mut session, "visits")
            .iter()
            .all(|(_, held)| *held == Value::Number(Number::Integer(0)))
    );
}

#[test]
fn a_required_field_without_a_default_is_still_refused_over_records_that_lack_it() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("ALTER TABLE users ADD FIELD visits TYPE int REQUIRED;")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("leaves required field visits"),
        "{refused}"
    );
}

#[test]
fn changing_a_default_later_reaches_only_later_writes() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE FIELD visits ON users TYPE int DEFAULT 0;")
        .unwrap();
    session.run_now("UPDATE users:1 SET visits = 5;").unwrap();

    session
        .run_now("ALTER TABLE users ALTER FIELD visits TYPE int DEFAULT 9;")
        .unwrap();

    assert_eq!(
        column(&mut session, "visits")
            .into_iter()
            .map(|(_, held)| held)
            .collect::<Vec<_>>(),
        [
            Value::Number(Number::Integer(5)),
            Value::Number(Number::Integer(0)),
            Value::Number(Number::Integer(0)),
        ]
    );
}

#[test]
fn filling_a_default_runs_no_event() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE COLLECTION audit;\n\
             DEFINE EVENT noted ON users THEN CREATE audit = { user: $id };",
        )
        .unwrap();

    session
        .run_now("DEFINE FIELD visits ON users TYPE int DEFAULT 0;")
        .unwrap();

    let audited = last(&mut session, "SELECT * FROM audit;");
    assert_eq!(audited.records().expect("records").len(), 0);
}

#[test]
fn altering_an_existing_field_does_not_fill_the_records_without_it() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE FIELD plan ON users TYPE string;")
        .unwrap();

    session
        .run_now("ALTER TABLE users ALTER FIELD plan TYPE string DEFAULT 'free';")
        .unwrap();

    // The field already existed, so its new default reaches later writes only:
    // the record that never had a plan still has none.
    assert_eq!(
        column(&mut session, "plan")[0],
        ("1".to_owned(), Value::None)
    );
}

#[test]
fn a_default_on_a_vault_holding_records_is_refused() {
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
        .run_now("DEFINE FIELD label ON secrets TYPE string DEFAULT 'x';")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("secrets is a vault holding records"),
        "{refused}"
    );
}
