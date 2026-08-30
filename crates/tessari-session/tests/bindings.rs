//! `LET` and `RETURN` — the two statements that let one engine's answer reach
//! the next statement.
//!
//! Every read in this language resolves to exactly one access path, chosen by
//! the shape of the statement. That is what keeps a read's cost legible, and it
//! is also why a question crossing two engines could not be *said*: the nearest
//! neighbours by embedding, and then who follows them, is a vector read and a
//! graph walk, and nothing carried the first answer into the second.
//!
//! A binding carries it. What these tests hold to is one sentence: **a bound
//! value behaves exactly as a caller-supplied one does** — it is substituted
//! into the statements below it before they run, so nothing downstream can tell
//! which of the two happened, and the planner still meets a literal where it
//! looks for one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A tenancy, three people, and the edges between them.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada', city: 'london', age: 36 };\n\
             CREATE users:2 = { name: 'grace', city: 'london', age: 45 };\n\
             CREATE users:3 = { name: 'alan', city: 'cambridge', age: 41 };",
        )
        .unwrap();
    session
}

fn rows(outcome: &Outcome) -> usize {
    outcome.records().map_or(0, <[_]>::len)
}

#[test]
fn a_binding_reaches_the_statement_below_it() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("LET $city = 'london'; SELECT * FROM users WHERE city = $city;")
        .unwrap();
    // The binding itself reports `Done`: its value is not the script's answer,
    // it is what the next statement was written against.
    assert_eq!(outcomes[0], Outcome::Done);
    assert_eq!(rows(&outcomes[1]), 2);
}

#[test]
fn a_bound_value_still_reaches_an_index() {
    // The property the whole design rests on. Substitution happens before the
    // statement runs, so the planner meets a literal on the right-hand side and
    // chooses the index — a binding resolved at evaluation instead would have
    // silently dropped it, and the read would still have answered correctly
    // while costing a scan.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "DEFINE INDEX by_city ON users FIELDS city;\n\
             LET $city = 'london';\n\
             SELECT * FROM users WHERE city = $city;",
        )
        .unwrap();
    assert_eq!(outcomes[2].path(), Some(AccessPath::Index));
}

#[test]
fn one_engines_answer_becomes_the_next_statements_question() {
    // The reason this feature exists. The first read resolves to one access
    // path and the second to another; what travels between them is a value.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "LET $names = SELECT name FROM users WHERE city = 'london';\n\
             RETURN $names;",
        )
        .unwrap();
    let Some(Value::Array(answered)) = outcomes[1].value() else {
        panic!("expected the bound array back, got {:?}", outcomes[1]);
    };
    assert_eq!(answered.len(), 2);
}

#[test]
fn a_binding_composes_with_a_fold() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("LET $floor = 40; SELECT count(*) AS n FROM users WHERE age > $floor;")
        .unwrap();
    let records = outcomes[1].records().unwrap();
    assert_eq!(records.len(), 1);
}

#[test]
fn a_return_names_the_scripts_answer() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("LET $a = 6; LET $b = 7; RETURN $a * $b;")
        .unwrap();
    assert_eq!(
        outcomes[2].value(),
        Some(&Value::Number(Number::Integer(42)))
    );
}

#[test]
fn a_binding_stands_where_a_record_identity_does() {
    // The same rule a caller's parameter follows: a value may stand for the
    // **id** half of an identity and never for the table half.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("LET $who = 2; SELECT * FROM users:$who;")
        .unwrap();
    assert_eq!(rows(&outcomes[1]), 1);
}

#[test]
fn a_name_is_bound_once_in_a_script() {
    let store = store();
    let mut session = ready(&store);
    let failed = session.run("LET $x = 1; LET $x = 2; RETURN $x;");
    assert!(
        format!("{}", failed.unwrap_err()).contains("bound twice"),
        "a rebinding should be refused where it is written"
    );
}

#[test]
fn a_script_answers_with_one_value() {
    let store = store();
    let mut session = ready(&store);
    let failed = session.run("RETURN 1; RETURN 2;");
    assert!(
        format!("{}", failed.unwrap_err()).contains("one value"),
        "two answers should be refused where they are written"
    );
}

#[test]
fn a_name_written_above_its_binding_is_refused_before_anything_runs() {
    // The property `Script::bind` already had, kept: a script either binds or
    // does nothing. `$x` here is bound by no caller and by no *preceding*
    // `LET`, so it fails before the write below it reaches the store.
    let store = store();
    let mut session = ready(&store);
    let failed = session.run(
        "CREATE users:9 = { name: $x };\n\
         LET $x = 'too late';",
    );
    assert!(failed.is_err(), "a forward reference should be refused");
    let mut session = Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let outcomes = session.run("SELECT * FROM users:9;").unwrap();
    assert_eq!(rows(&outcomes[0]), 0, "nothing should have been written");
}

#[test]
fn a_caller_and_a_script_may_not_name_the_same_thing() {
    // Refused rather than resolved by precedence: either reading silently
    // discards a value somebody supplied on purpose.
    let store = store();
    let mut session = ready(&store);
    let mut supplied = Parameters::new();
    supplied.insert("city".to_owned(), Value::String("london".to_owned()));
    let failed = session.run_with(
        "LET $city = 'cambridge'; SELECT * FROM users WHERE city = $city;",
        &supplied,
    );
    assert!(
        format!("{}", failed.unwrap_err()).contains("supplied by the caller"),
        "a collision should name both halves"
    );
}

#[test]
fn a_binding_and_a_caller_value_compose() {
    let store = store();
    let mut session = ready(&store);
    let mut supplied = Parameters::new();
    supplied.insert("floor".to_owned(), Value::Number(Number::Integer(40)));
    let outcomes = session
        .run_with(
            "LET $city = 'london'; SELECT * FROM users WHERE city = $city AND age > $floor;",
            &supplied,
        )
        .unwrap();
    assert_eq!(rows(&outcomes[1]), 1);
}

#[test]
fn a_binding_survives_a_transaction() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run(
            "BEGIN;\n\
             LET $city = 'cambridge';\n\
             CREATE users:4 = { name: 'joan', city: $city, age: 29 };\n\
             COMMIT;\n\
             SELECT * FROM users WHERE city = 'cambridge';",
        )
        .unwrap();
    assert_eq!(rows(&outcomes[4]), 2);
}
