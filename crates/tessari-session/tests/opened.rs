//! `SPLIT ON <route>` — one record per element of an array field.
//!
//! # The failure
//!
//! An array field could not be unnested, so `GROUP BY tags` grouped by the whole
//! array and *"each tag, and how many notes carry it"* — the first question
//! anybody asks of a tagged document — was unsayable in a document store.
//!
//! # What the tests here are actually pinning
//!
//! Not that the words parse. The four shapes a route can reach, which is where
//! every decision in this clause lives:
//!
//! - **an array** is the case the clause is for, and the identity rides onto
//!   every row it produces;
//! - **an empty array** answers with no rows, because zero elements is zero
//!   rows and any other rule makes the count depend on a special case;
//! - **an absence and a scalar** pass through once, because an array says what
//!   the elements are and an absence says nothing about elements at all;
//! - and the clause composes with the grouping that is the reason to want it.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE TABLE notes;
CREATE notes:1 = { title: 'first', tags: ['rust', 'db'] };
CREATE notes:2 = { title: 'second', tags: ['db'] };
CREATE notes:3 = { title: 'third', tags: [] };
CREATE notes:4 = { title: 'fourth' };
CREATE notes:5 = { title: 'fifth', tags: 'not-a-list' };
DEFINE TABLE people;
CREATE people:1 = { name: 'ada', address: { city: 'london', tags: ['a', 'b'] } };
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

fn rows(answered: &Outcome) -> usize {
    answered.records().expect("records").len()
}

fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Object(fields) = record else {
        panic!("not an object: {record:?}")
    };
    fields.get(name)
}

/// Every value one field holds across the answer, in order.
fn column(answered: &Outcome, name: &str) -> Vec<Value> {
    answered
        .records()
        .expect("records")
        .iter()
        .map(|(_, record)| field(record, name).cloned().unwrap_or(Value::None))
        .collect()
}

#[test]
fn an_array_becomes_one_record_per_element() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'first' SPLIT ON tags;",
    );
    assert_eq!(rows(&answered), 2);
    assert_eq!(
        column(&answered, "tags"),
        vec![Value::from("rust"), Value::from("db")],
        "the element did not replace the array it came from"
    );
    // Everything else rides along untouched — the row is the record with one
    // field opened, not a projection of it.
    assert_eq!(
        column(&answered, "title"),
        vec![Value::from("first"), Value::from("first")]
    );
}

#[test]
fn the_identity_rides_onto_every_row_the_split_produced() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'first' SPLIT ON tags;",
    );
    let records = answered.records().expect("records");
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].0, records[1].0,
        "two rows from one record answered under two identities"
    );
}

#[test]
fn an_empty_array_answers_with_no_rows_at_all() {
    let store = store();
    let mut session = ready(&store);
    // Zero elements, zero rows. Any other rule makes the count depend on a
    // special case rather than on the clause.
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'third' SPLIT ON tags;",
    );
    assert_eq!(rows(&answered), 0);
}

#[test]
fn an_absent_field_passes_through_once_because_it_is_not_an_empty_array() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'fourth' SPLIT ON tags;",
    );
    assert_eq!(rows(&answered), 1);
    assert_eq!(column(&answered, "title"), vec![Value::from("fourth")]);
}

#[test]
fn a_value_that_is_not_a_list_passes_through_once_rather_than_refusing() {
    let store = store();
    let mut session = ready(&store);
    // A field's kind is per record here, not per table. Refusing would mean one
    // record in ten thousand deciding the whole read.
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'fifth' SPLIT ON tags;",
    );
    assert_eq!(rows(&answered), 1);
    assert_eq!(column(&answered, "tags"), vec![Value::from("not-a-list")]);
}

#[test]
fn the_whole_table_splits_to_the_sum_of_its_elements_and_its_unopened_records() {
    let store = store();
    let mut session = ready(&store);
    // 2 + 1 elements, 0 for the empty array, and 1 each for the absence and the
    // scalar.
    let answered = run(&mut session, "SELECT * FROM notes SPLIT ON tags;");
    assert_eq!(rows(&answered), 5);
}

#[test]
fn each_tag_and_how_many_notes_carry_it_is_the_question_this_clause_exists_for() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT tags, count(*) AS n FROM notes SPLIT ON tags GROUP BY tags;",
    );
    // `rust` once, `db` twice, `not-a-list` once, and one group for the record
    // holding no tags at all.
    assert_eq!(rows(&answered), 4);
    let counts = column(&answered, "n");
    assert_eq!(
        counts.iter().filter(|n| **n == Value::from(2i64)).count(),
        1,
        "the tag two notes carry was not counted twice: {counts:?}"
    );
}

#[test]
fn a_route_reaches_an_array_inside_the_record() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT * FROM people SPLIT ON address.tags;");
    assert_eq!(rows(&answered), 2);
    let records = answered.records().expect("records");
    let address = field(&records[0].1, "address").expect("the address survived");
    assert_eq!(
        field(address, "tags"),
        Some(&Value::from("a")),
        "the element did not land inside the record it came from"
    );
    assert_eq!(
        field(address, "city"),
        Some(&Value::from("london")),
        "opening the route rebuilt the object around it"
    );
}

#[test]
fn an_ordering_sees_the_rows_the_split_produced_and_not_the_records() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'first' SPLIT ON tags ORDER BY tags;",
    );
    assert_eq!(
        column(&answered, "tags"),
        vec![Value::from("db"), Value::from("rust")],
        "the ordering ran above the split instead of below it"
    );
}

#[test]
fn a_bound_is_applied_to_the_rows_and_not_to_the_records() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * FROM notes WHERE title = 'first' SPLIT ON tags LIMIT 1;",
    );
    assert_eq!(rows(&answered), 1);
}

#[test]
fn the_word_split_is_still_a_field_name() {
    let store = store();
    let mut session = ready(&store);
    run(
        &mut session,
        "DEFINE TABLE runs; CREATE runs:1 = { split: 'yes' };",
    );
    let answered = run(&mut session, "SELECT split FROM runs;");
    assert_eq!(column(&answered, "split"), vec![Value::from("yes")]);
}
