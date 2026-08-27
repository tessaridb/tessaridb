//! `IF … THEN … ELSE … END` and `??` — computing a value that depends on a test.
//!
//! Both are **expressions**, which is the whole design decision: what was
//! missing was not control flow but the ability to work a value out
//! conditionally in the four places a value stands — a projection, an
//! assignment, a filter and an ordering. A statement form would have served
//! none of them.
//!
//! `??` is the one place this language treats `NONE` and `NULL` alike, and the
//! tests below pin both halves of that: the two are the same to `??`, and they
//! stay different to everything else.

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

/// Four people: one with everything, one whose nickname is `null`, one with no
/// nickname at all, and one to keep the counts honest.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'ada', nickname: 'countess', age: 36 };\n\
             CREATE users:2 = { name: 'grace', nickname: NULL, age: 45 };\n\
             CREATE users:3 = { name: 'alan', age: 41 };\n\
             CREATE users:4 = { name: 'joan', nickname: 'joanie', age: 29 };",
        )
        .unwrap();
    session
}

fn one(outcome: &Outcome) -> &Value {
    let records = outcome.records().expect("records");
    assert_eq!(records.len(), 1, "expected one record, got {records:?}");
    &records[0].1
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(fields) = value else {
        panic!("not an object: {value:?}");
    };
    fields
        .get(name)
        .unwrap_or_else(|| panic!("no {name} in {fields:?}"))
}

#[test]
fn a_conditional_computes_a_projected_value() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "SELECT IF age >= 40 THEN 'senior' ELSE 'junior' END AS band \
             FROM users WHERE name = 'ada';",
        )
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "band"),
        &Value::String("junior".to_owned())
    );
}

#[test]
fn else_if_chains_and_one_end_closes_it() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "SELECT IF age >= 45 THEN 'a' ELSE IF age >= 40 THEN 'b' ELSE 'c' END AS band \
             FROM users WHERE name = 'alan';",
        )
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "band"),
        &Value::String("b".to_owned())
    );
}

#[test]
fn a_conditional_with_no_else_answers_an_absence() {
    // Not `null`, and not a key holding `none` — an **absence**, which the
    // answer expresses by not carrying the key at all. That is exactly what a
    // route into a field the record does not have already does, which is the
    // point: the two absences compose rather than needing a rule apiece.
    let store = store();
    let mut session = ready(&store);
    let conditional = session
        .run("SELECT IF age > 100 THEN 'ancient' END AS band FROM users WHERE name = 'ada';")
        .unwrap();
    let missing_path = session
        .run("SELECT no_such_field AS band FROM users WHERE name = 'ada';")
        .unwrap();
    assert_eq!(
        one(&conditional[0]),
        one(&missing_path[0]),
        "an untaken conditional and a missing field should answer alike"
    );
    let Value::Object(fields) = one(&conditional[0]) else {
        panic!("not an object");
    };
    assert!(
        fields.is_empty(),
        "an absence should not carry a key: {fields:?}"
    );
}

#[test]
fn only_the_taken_arm_is_evaluated() {
    // The untaken arm divides by zero. If both arms ran, this would fail —
    // which is the point: an arm that is not taken need not be meaningful for
    // the record it is not taken on.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT IF age > 0 THEN age ELSE age / 0 END AS n FROM users WHERE name = 'ada';")
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "n"),
        &Value::Number(Number::Integer(36))
    );
}

#[test]
fn a_conditional_stands_in_a_filter() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT * FROM users WHERE (IF age >= 40 THEN true ELSE false END) = true;")
        .unwrap();
    assert_eq!(outcomes[0].records().map(<[_]>::len), Some(2));
}

#[test]
fn a_conditional_stands_in_an_ordering() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT name FROM users ORDER BY IF age >= 40 THEN 0 ELSE 1 END, name;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(
        field(&records[0].1, "name"),
        &Value::String("alan".to_owned())
    );
}

#[test]
fn a_conditional_stands_in_an_assignment() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("UPDATE users:3 SET band = IF age >= 40 THEN 'senior' ELSE 'junior' END;")
        .unwrap();
    let outcomes = session.run("SELECT band FROM users:3;").unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "band"),
        &Value::String("senior".to_owned())
    );
}

#[test]
fn coalesce_passes_over_an_absent_field() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT nickname ?? name AS shown FROM users WHERE name = 'alan';")
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "shown"),
        &Value::String("alan".to_owned())
    );
}

#[test]
fn and_over_a_null_one_too() {
    // The one place `NONE` and `NULL` are alike: the question is whether there
    // is a value to use, and the answer is no in both cases.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT nickname ?? name AS shown FROM users WHERE name = 'grace';")
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "shown"),
        &Value::String("grace".to_owned())
    );
}

#[test]
fn and_keeps_a_value_that_is_there() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT nickname ?? name AS shown FROM users WHERE name = 'ada';")
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "shown"),
        &Value::String("countess".to_owned())
    );
}

#[test]
fn none_and_null_stay_different_everywhere_else() {
    // The other half of the rule above. `??` collapsing them must not have
    // taught anything else to.
    let store = store();
    let mut session = ready(&store);
    let absent = session
        .run("SELECT * FROM users WHERE nickname = NONE;")
        .unwrap();
    let null = session
        .run("SELECT * FROM users WHERE nickname = NULL;")
        .unwrap();
    assert_eq!(absent[0].records().map(<[_]>::len), Some(1));
    assert_eq!(null[0].records().map(<[_]>::len), Some(1));
}

#[test]
fn coalesce_chains_left_to_right() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT nickname ?? missing ?? 'unknown' AS shown FROM users WHERE name = 'alan';")
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "shown"),
        &Value::String("unknown".to_owned())
    );
}

#[test]
fn coalesce_binds_tighter_than_a_comparison() {
    // `nickname ?? name = 'alan'` must ask what it looks like it asks:
    // `(nickname ?? name) = 'alan'`, which holds for exactly one record.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT * FROM users WHERE nickname ?? name = 'alan';")
        .unwrap();
    assert_eq!(outcomes[0].records().map(<[_]>::len), Some(1));
}

#[test]
fn coalesce_binds_looser_than_arithmetic() {
    // `missing ?? 1 + 1` is `missing ?? (1 + 1)` and answers 2, not 1.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT missing ?? 1 + 1 AS n FROM users WHERE name = 'ada';")
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "n"),
        &Value::Number(Number::Integer(2))
    );
}

#[test]
fn the_two_compose() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "SELECT IF (nickname ?? '') = '' THEN 'no nickname' ELSE nickname END AS shown \
             FROM users WHERE name = 'grace';",
        )
        .unwrap();
    assert_eq!(
        field(one(&outcomes[0]), "shown"),
        &Value::String("no nickname".to_owned())
    );
}

#[test]
fn a_conditional_needs_its_end() {
    let store = store();
    let mut session = ready(&store);
    let failed = session.run("SELECT IF age > 1 THEN 'a' ELSE 'b' AS band FROM users;");
    assert!(
        failed.is_err(),
        "a conditional without END should be refused"
    );
}

#[test]
fn a_test_that_is_not_a_boolean_is_refused() {
    let store = store();
    let mut session = ready(&store);
    let failed = session.run("SELECT IF name THEN 'a' ELSE 'b' END AS band FROM users;");
    assert!(failed.is_err(), "a non-boolean test should be refused");
}

#[test]
fn a_lone_question_mark_is_not_an_operator() {
    // A value is written `$name` in this language, so a single `?` is a typo
    // and is refused where it is written rather than further along.
    let store = store();
    let mut session = ready(&store);
    let failed = session.run("SELECT nickname ? name AS shown FROM users;");
    assert!(failed.is_err(), "a lone `?` should be refused");
}
