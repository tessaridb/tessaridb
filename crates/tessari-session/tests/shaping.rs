//! `SELECT *` composes, and `OMIT` subtracts from what it put there.
//!
//! # The failure
//!
//! `SELECT *` and `SELECT a, b` were mutually exclusive, so "the record, plus one
//! computed column" — the commonest shape in day-to-day SQL — meant listing every
//! field by hand, and that list broke the moment a field was added.
//!
//! `OMIT` is the other half and it is the one this store needs most: with a
//! vector engine in the box, a `SELECT *` over a table with an embedding ships a
//! wall of floats to the client on every row, and the only escape was to
//! enumerate every *other* field — which is the fragile list again.
//!
//! # What the tests here are actually pinning
//!
//! Not that the words parse. Three rules that a passing parse would hide:
//! a name written out **wins** over the field the star offered under the same
//! name; `OMIT` reaches inside a record without taking the field that holds the
//! route; and a bare `SELECT *` still takes the path that copies nothing.

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
CREATE people:1 = { name: 'ada', age: 36, address: { city: 'london', postcode: 'N1' } };
CREATE people:2 = { name: 'grace', age: 45, address: { city: 'york', postcode: 'YO1' } };
DEFINE COLLECTION notes;
CREATE notes:1 = { title: 'first', embedding: [0.1, 0.2, 0.3] };
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

/// The first record a read answered with.
fn first(answered: &Outcome) -> &Value {
    let records = answered.records().expect("records");
    &records.first().expect("at least one record").1
}

/// The field names one record answers under, in order.
fn names(record: &Value) -> Vec<&str> {
    let Value::Object(fields) = record else {
        panic!("not an object")
    };
    fields.keys().map(String::as_str).collect()
}

fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Object(fields) = record else {
        panic!("not an object")
    };
    fields.get(name)
}

/// The refusal, as a caller reads it.
fn refusal(session: &mut Session<'_>, script: &str) -> String {
    session
        .run(script)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("{script}: answered instead of refusing"))
}

#[test]
fn a_bare_star_still_answers_with_the_record_as_it_is() {
    let store = store();
    let mut session = ready(&store);
    // The read that copies nothing, and the commonest one in the language. It
    // keeps its own path precisely so composing does not cost it anything.
    let answered = run(&mut session, "SELECT * FROM people WHERE name = 'ada';");
    assert_eq!(names(first(&answered)), ["address", "age", "name"]);
}

#[test]
fn a_star_and_a_computed_value_answer_together() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT *, age + 1 AS next FROM people WHERE name = 'ada';",
    );
    assert_eq!(names(first(&answered)), ["address", "age", "name", "next"]);
    assert_eq!(field(first(&answered), "next"), Some(&Value::from(37)));
}

#[test]
fn a_name_written_out_wins_over_the_field_the_star_offered() {
    let store = store();
    let mut session = ready(&store);
    // The rule that has to be decided rather than discovered: both halves offer
    // `name`, and the one the author typed is the one they meant. It is the same
    // rule the ordering stage's overlay already follows.
    let answered = run(
        &mut session,
        "SELECT *, 'shouted' AS name FROM people WHERE age = 36;",
    );
    assert_eq!(names(first(&answered)), ["address", "age", "name"]);
    assert_eq!(
        field(first(&answered), "name"),
        Some(&Value::from("shouted")),
    );
}

#[test]
fn omit_takes_a_field_out_of_what_the_star_put_there() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT * OMIT age FROM people WHERE name = 'ada';",
    );
    assert_eq!(names(first(&answered)), ["address", "name"]);
}

#[test]
fn omit_reaches_inside_without_taking_the_field_that_holds_the_route() {
    let store = store();
    let mut session = ready(&store);
    // `OMIT address.postcode` keeps the address. This is the half a top-level
    // filter cannot do, and the reason `OMIT` takes a route rather than a name.
    let answered = run(
        &mut session,
        "SELECT * OMIT address.postcode FROM people WHERE name = 'ada';",
    );
    let Some(Value::Object(address)) = field(first(&answered), "address") else {
        panic!("the address went with its postcode")
    };
    assert_eq!(
        address.keys().map(String::as_str).collect::<Vec<_>>(),
        ["city"],
    );
}

#[test]
fn omit_on_a_vector_field_keeps_the_floats_out_of_the_answer() {
    let store = store();
    let mut session = ready(&store);
    // The case this clause exists for: without it, every read of a table with an
    // embedding ships the embedding, and the only escape is to list every other
    // field — which is the list that breaks when a field is added.
    let answered = run(&mut session, "SELECT * OMIT embedding FROM notes;");
    assert_eq!(names(first(&answered)), ["title"]);
}

#[test]
fn omit_subtracts_from_the_star_and_not_from_what_was_written_out() {
    let store = store();
    let mut session = ready(&store);
    // `age` leaves as the star's contribution and comes back as a name the
    // author wrote, which is what "subtracts from what the star put there" has
    // to mean if the two halves are to compose at all.
    let answered = run(
        &mut session,
        "SELECT *, age + 1 AS age OMIT age FROM people WHERE name = 'ada';",
    );
    assert_eq!(field(first(&answered), "age"), Some(&Value::from(37)));
}

#[test]
fn an_order_key_still_reads_a_field_the_omit_removed() {
    let store = store();
    let mut session = ready(&store);
    // The defect this would otherwise reintroduce: a key evaluated against the
    // answer alone cannot see what `OMIT` took out, every record ties, and the
    // read answers in whatever order the source produced — with no error
    // anywhere. The overlay is built when the read stars *and* omits.
    let answered = run(
        &mut session,
        "SELECT * OMIT age FROM people ORDER BY age DESC LIMIT 2;",
    );
    let records = answered.records().unwrap();
    assert_eq!(
        records
            .iter()
            .map(|(_, record)| field(record, "name").cloned())
            .collect::<Vec<_>>(),
        vec![Some(Value::from("grace")), Some(Value::from("ada"))],
    );
    assert_eq!(names(&records[0].1), ["address", "name"]);
}

#[test]
fn omit_with_no_star_to_subtract_from_is_refused() {
    let store = store();
    let mut session = ready(&store);
    // Refused rather than accepted and ignored: with nothing to subtract from,
    // the clause is either a mistake about what the read answers with or a
    // request to drop a value the author wrote out on purpose.
    let said = refusal(&mut session, "SELECT name OMIT age FROM people;");
    assert!(said.contains("`*`"), "{said}");
}

#[test]
fn omit_cannot_leave_out_a_position() {
    let store = store();
    let mut session = ready(&store);
    // Removing an element renumbers everything after it, so what the answer held
    // at position one would depend on what was left out. Refused rather than
    // guessed at.
    let said = refusal(&mut session, "SELECT * OMIT embedding[0] FROM notes;");
    assert!(said.contains("renumber"), "{said}");
}

#[test]
fn a_second_star_is_refused() {
    let store = store();
    let mut session = ready(&store);
    // It adds nothing the first did not, so it is a typo rather than a meaning.
    let said = refusal(&mut session, "SELECT *, * FROM people;");
    assert!(said.contains('*'), "{said}");
}

#[test]
fn the_word_omit_is_still_a_field_name() {
    let store = store();
    let mut session = ready(&store);
    // Every clause word in this grammar is contextual, and this one is no
    // exception: a projection reading `omit` as a value consumes it before the
    // clause is ever looked for.
    session
        .run("CREATE people:3 = { name: 'alan', omit: 'a word, not a clause' };")
        .unwrap();
    let answered = run(&mut session, "SELECT omit FROM people WHERE name = 'alan';");
    assert_eq!(
        field(first(&answered), "omit"),
        Some(&Value::from("a word, not a clause")),
    );
}
