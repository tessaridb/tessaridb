//! `compared-across-kinds` — the read says when it compared a number with text.
//!
//! # The failure
//!
//! A schemaless store lets one record hold `age: 30` and the next `age: '30'`,
//! and `WHERE age = 30` then matches some of them. Nothing goes wrong: the
//! comparison is well defined, the answer is right for the values that are there,
//! and the read quietly answers a narrower question than the one that was asked.
//!
//! # The half that is harder than the note
//!
//! Not firing. A note that fires on the ordinary read is worse than no note,
//! because it looks like a feature — slice U1 built exactly that mistake once and
//! caught it, keying a note on a shape the planner matched rather than on an
//! index that existed and declined. Here the ordinary read is the **absent
//! field**: a record without `age` compares `none` against `30`, which is how a
//! schemaless read narrows instead of failing, and is not a mistake. So there are
//! more silence tests here than noise tests, and they come first.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Note, Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// One table holding the same field as a number, as text, and not at all.
const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE TABLE people;
DEFINE TABLE sparse;
CREATE sparse:1 = { a: 1 };
CREATE sparse:2 = { b: 2 };
CREATE people:1 = { name: 'ada', age: 30 };
CREATE people:2 = { name: 'grace', age: 45 };
CREATE people:3 = { name: 'alan', age: '30' };
CREATE people:4 = { name: 'edsger' };
";

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

/// The kinds a read said it compared across, in the order the note lists them.
fn crossed(outcome: &Outcome) -> Vec<(&'static str, &'static str)> {
    outcome
        .notes()
        .iter()
        .filter_map(|note| match note {
            Note::ComparedAcrossKinds { left, right } => Some((*left, *right)),
            _ => None,
        })
        .collect()
}

fn read(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

#[test]
fn an_absent_field_is_not_a_mismatch() {
    let store = store();
    let mut session = ready(&store);
    // `sparse:2` has no `a`, so it compares `none` with `1`. That is the
    // ordinary schemaless read and the whole reason the absence rule exists —
    // noting it would put a note on nearly every read in the language. The table
    // is its own, so the absence is the *only* thing this read crosses.
    let answered = read(&mut session, "SELECT * FROM sparse WHERE a = 1;");
    assert_eq!(answered.records().unwrap().len(), 1);
    assert_eq!(crossed(&answered), Vec::new(), "an absence was noted");
}

#[test]
fn a_read_that_compares_one_kind_says_nothing() {
    let store = store();
    let mut session = ready(&store);
    let answered = read(&mut session, "SELECT * FROM people WHERE name = 'ada';");
    assert_eq!(answered.records().unwrap().len(), 1);
    assert_eq!(crossed(&answered), Vec::new());
}

#[test]
fn a_read_with_no_condition_at_all_says_nothing() {
    let store = store();
    let mut session = ready(&store);
    let answered = read(&mut session, "SELECT * FROM people;");
    assert_eq!(answered.records().unwrap().len(), 4);
    assert_eq!(crossed(&answered), Vec::new());
}

#[test]
fn a_number_compared_with_the_text_of_one_is_noted() {
    let store = store();
    let mut session = ready(&store);
    let answered = read(&mut session, "SELECT * FROM people WHERE age = 30;");
    // The answer is still right — `people:1` genuinely holds the number 30 — and
    // that is exactly why silence would be wrong: `people:3` holds `'30'` and the
    // author almost certainly meant it too.
    assert_eq!(answered.records().unwrap().len(), 1);
    assert_eq!(crossed(&answered), vec![("number", "string")]);
}

#[test]
fn the_note_names_the_kinds_in_one_order_whichever_side_they_were_written_on() {
    let store = store();
    let mut session = ready(&store);
    // A mismatch is a fact about a pair, not about which side of the `=` each
    // half was typed on, so the two statements say the same thing.
    let left = read(&mut session, "SELECT * FROM people WHERE age = '30';");
    let right = read(&mut session, "SELECT * FROM people WHERE '30' = age;");
    assert_eq!(crossed(&left), vec![("number", "string")]);
    assert_eq!(crossed(&right), crossed(&left));
}

#[test]
fn a_million_mismatched_records_are_one_note() {
    let store = store();
    let mut session = ready(&store);
    let mut script = String::new();
    for n in 1..=200 {
        script.push_str(&format!("CREATE people:{} = {{ age: '{n}' }};\n", n + 100));
    }
    session.run(&script).unwrap();
    let answered = read(&mut session, "SELECT * FROM people WHERE age = 30;");
    // A comparison runs once per record, so a read that mixes two kinds has one
    // thing to say and not one thing per record. A note repeated two hundred
    // times is a note nobody reads.
    assert_eq!(crossed(&answered), vec![("number", "string")]);
}

#[test]
fn a_read_that_crosses_two_pairs_says_both() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE people:5 = { name: 7, age: true };")
        .unwrap();
    let answered = read(
        &mut session,
        "SELECT * FROM people WHERE age = 30 OR name = 'ada';",
    );
    let mut crossed = crossed(&answered);
    crossed.sort_unstable();
    // `age` meets a bool and a string; `name` meets a number. Each *pair* once,
    // which is why the number/string crossing appears here only once even though
    // two fields produced it.
    assert_eq!(crossed, vec![("bool", "number"), ("number", "string")]);
}

#[test]
fn a_comparison_written_in_an_order_key_is_noticed_too() {
    let store = store();
    let mut session = ready(&store);
    // The shaping stage evaluates the projection and the order's keys, and a
    // comparison there is as able to cross kinds as one in the `WHERE`. It runs
    // through a different consumer, which is why it is worth its own test.
    let answered = read(
        &mut session,
        "SELECT name FROM people ORDER BY age = '30' LIMIT 4;",
    );
    assert_eq!(answered.records().unwrap().len(), 4);
    assert_eq!(crossed(&answered), vec![("number", "string")]);
}

#[test]
fn a_note_belongs_to_the_read_that_earned_it_and_not_the_next_one() {
    let store = store();
    let mut session = ready(&store);
    // The sink is owned by the read and borrowed by what it evaluates, so it
    // cannot outlive the statement. This is the property that made the note
    // channel worth building at all: a note reported against the *next* answer
    // is worse than no note.
    let noted = read(&mut session, "SELECT * FROM people WHERE age = 30;");
    assert_eq!(crossed(&noted), vec![("number", "string")]);
    let clean = read(&mut session, "SELECT * FROM people WHERE name = 'ada';");
    assert_eq!(crossed(&clean), Vec::new(), "a note outlived its read");
}
