//! `count(DISTINCT x)` (ADR-0134 D8): how many distinct present values, exactly.

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

/// Visits from three cities, `paris` three times; one visit has no city, one a
/// `NULL` city, and two cities are the same number written two ways.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION visits;\n\
             CREATE visits:1 = { who: 'ada', city: 'paris', zone: 3 };\n\
             CREATE visits:2 = { who: 'ada', city: 'paris', zone: 3.0 };\n\
             CREATE visits:3 = { who: 'ada', city: 'rome', zone: 4 };\n\
             CREATE visits:4 = { who: 'grace', city: 'paris' };\n\
             CREATE visits:5 = { who: 'grace', city: NULL };\n\
             CREATE visits:6 = { who: 'grace' };",
        )
        .unwrap();
    session
}

/// Each row, in answer order.
fn rows(session: &mut Session<'_>, statement: &str) -> Vec<Value> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records.into_iter().map(|(_, row)| row).collect(),
        other => panic!("records, got {other:?}"),
    }
}

fn field<'a>(row: &'a Value, name: &str) -> &'a Value {
    match row {
        Value::Object(fields) => fields.get(name).unwrap_or(&Value::None),
        other => panic!("a row, got {other:?}"),
    }
}

fn integer(held: i64) -> Value {
    Value::Number(Number::Integer(held))
}

#[test]
fn counts_each_present_value_once() {
    let store = store();
    let mut session = ready(&store);

    let answer = rows(
        &mut session,
        "SELECT count(DISTINCT city) AS cities, count(city) AS visits FROM visits;",
    );

    // `paris` and `rome`; the absent and the `NULL` city count for nothing,
    // exactly as `count(city)` passes over them.
    assert_eq!(field(&answer[0], "cities"), &integer(2));
    assert_eq!(field(&answer[0], "visits"), &integer(4));
}

#[test]
fn equal_numbers_are_one_value() {
    let store = store();
    let mut session = ready(&store);

    let answer = rows(
        &mut session,
        "SELECT count(DISTINCT zone) AS zones FROM visits;",
    );

    // `3` and `3.0` are equal by `=`, so they are one zone.
    assert_eq!(field(&answer[0], "zones"), &integer(2));
}

#[test]
fn counts_within_each_group() {
    let store = store();
    let mut session = ready(&store);

    let answer = rows(
        &mut session,
        "SELECT who, count(DISTINCT city) AS cities FROM visits GROUP BY who;",
    );

    assert_eq!(answer.len(), 2);
    assert_eq!(field(&answer[0], "who"), &Value::from("ada"));
    assert_eq!(field(&answer[0], "cities"), &integer(2));
    assert_eq!(field(&answer[1], "who"), &Value::from("grace"));
    assert_eq!(field(&answer[1], "cities"), &integer(1));
}

#[test]
fn over_nothing_it_is_zero() {
    let store = store();
    let mut session = ready(&store);

    let answer = rows(
        &mut session,
        "SELECT count(*) AS visits, count(DISTINCT missing) AS cities FROM visits;",
    );

    // Six records, none holding `missing`.
    assert_eq!(field(&answer[0], "visits"), &integer(6));
    assert_eq!(field(&answer[0], "cities"), &integer(0));
}

#[test]
fn a_field_named_distinct_is_still_counted() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE visits:7 = { distinct: true };")
        .unwrap();

    let answer = rows(
        &mut session,
        "SELECT count(distinct) AS marked FROM visits;",
    );

    assert_eq!(field(&answer[0], "marked"), &integer(1));
}

#[test]
fn distinct_records_are_not_a_question_it_answers() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT count(DISTINCT *) AS n FROM visits;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("`count(*)` counts records"), "{refused}");
}
