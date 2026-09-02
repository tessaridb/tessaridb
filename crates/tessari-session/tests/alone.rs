//! `ONLY` — a read that says at most one record answers it.
//!
//! # The failure
//!
//! A read by identity answered with a list of one, so every caller unwrapped it
//! by hand, and the unwrap was written once per call site rather than once.
//!
//! # What the tests here are actually pinning
//!
//! Not that the word parses. Four decisions a passing parse would hide:
//!
//! - **none and more-than-one are not the same mistake.** `ONLY` asserts *at
//!   most* one, so an absence is a legitimate answer to a question about one
//!   thing and more than one is a false assertion;
//! - **more than one refuses rather than answering with the first**, which is
//!   the shape a truncating default would have had — cheap, and indistinguishable
//!   from success;
//! - **the bound is applied first**, so `ONLY … LIMIT 1` is the author saying
//!   which one rather than contradicting themselves;
//! - **an expression position answers with the record**, which is the half a
//!   source-shaped rule cannot reach: `FROM ONLY users WHERE …` is a read whose
//!   source could have answered with many.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION people;
CREATE people:1 = { name: 'ada', email: 'ada@example.com', city: 'london' };
CREATE people:2 = { name: 'grace', email: 'grace@example.com', city: 'york' };
CREATE people:3 = { name: 'alan', email: 'alan@example.com', city: 'london' };
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

/// The refusal, as a caller reads it.
fn refusal(session: &mut Session<'_>, script: &str) -> String {
    session
        .run(script)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("{script}: answered instead of refusing"))
}

fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Object(fields) = record else {
        panic!("not an object: {record:?}")
    };
    fields.get(name)
}

#[test]
fn a_read_by_identity_says_it_answers_with_one() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM ONLY people:1;");
    assert!(answered.only(), "the answer did not carry the assertion");
    let records = answered.records().expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(
        field(&records[0].1, "name"),
        Some(&Value::from("ada")),
        "the wrong record came back"
    );
}

#[test]
fn a_condition_that_matches_one_is_the_case_no_source_rule_could_reach() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM ONLY people WHERE email = 'grace@example.com';",
    );
    assert!(answered.only());
    let records = answered.records().expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0].1, "name"), Some(&Value::from("grace")));
}

#[test]
fn more_than_one_is_refused_and_the_refusal_says_how_many() {
    let store = store();
    let mut session = ready(&store);
    let message = refusal(
        &mut session,
        "SELECT * FROM ONLY people WHERE city = 'london';",
    );
    assert!(
        message.contains("`ONLY`") && message.contains('2'),
        "the refusal did not say how many answered: {message}"
    );
}

#[test]
fn a_whole_table_of_more_than_one_is_refused_rather_than_truncated() {
    let store = store();
    let mut session = ready(&store);
    let message = refusal(&mut session, "SELECT * FROM ONLY people;");
    assert!(
        message.contains('3'),
        "a prefix was answered instead of the read being refused: {message}"
    );
}

#[test]
fn none_answers_with_nothing_rather_than_refusing() {
    let store = store();
    let mut session = ready(&store);
    // At most one, not exactly one. Refusing here would make
    // `SELECT * FROM ONLY people:99 ?? { }` unsayable.
    let answered = run(&mut session, "SELECT * FROM ONLY people:99;");
    assert!(answered.only());
    assert!(answered.records().expect("records").is_empty());
}

#[test]
fn a_bound_is_applied_before_the_assertion_is_tested() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM ONLY people LIMIT 1;");
    assert!(answered.only());
    assert_eq!(answered.records().expect("records").len(), 1);
}

#[test]
fn a_read_in_an_expression_answers_with_the_record_and_not_a_list_of_one() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "RETURN (SELECT name FROM ONLY people WHERE email = 'alan@example.com');",
    );
    let held = answered.value().expect("a value");
    assert_eq!(
        field(held, "name"),
        Some(&Value::from("alan")),
        "the expression answered with something other than the record: {held:?}"
    );
}

#[test]
fn the_same_read_without_only_still_answers_with_a_list() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "RETURN (SELECT name FROM people WHERE email = 'alan@example.com' LIMIT 1);",
    );
    let Some(Value::Array(held)) = answered.value() else {
        panic!("the list disappeared without the word being written")
    };
    assert_eq!(held.len(), 1);
}

#[test]
fn a_read_that_did_not_say_the_word_does_not_claim_it() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM people:1;");
    assert!(
        !answered.only(),
        "a read that never said `ONLY` answered as though it had"
    );
}

#[test]
fn only_is_reserved_so_a_table_of_that_name_is_not_addressable() {
    let store = store();
    let mut session = ready(&store);
    // The cost of the decision, pinned rather than left to be discovered: the
    // word is reserved because `FROM only limit 1` cannot be told apart from
    // this marker in front of a table called `limit`.
    let message = refusal(&mut session, "DEFINE COLLECTION only;");
    assert!(
        !message.is_empty(),
        "`only` was accepted as a table name after all"
    );
}
