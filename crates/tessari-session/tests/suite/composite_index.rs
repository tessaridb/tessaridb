//! A composite index that is actually read.
//!
//! `EXPLAIN` found this one wave after it became possible to ask: an index with
//! more than one field was offered for **nothing** — not a leading equality, not
//! a range, not even a condition naming both its columns — while being
//! maintained on every write. A cost with no benefit, and it raised nothing for
//! as long as it existed, because a wrong cost never does.
//!
//! The rule now is the one the key order already implies: an index serves a
//! condition on its **first** field. Only the first — the entries for one value
//! of a later field are scattered across every value of the ones before it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Six people whose surnames repeat, so a leading lookup finds more than one.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { last: 'lovelace', first: 'ada' };\n\
             CREATE users:2 = { last: 'lovelace', first: 'byron' };\n\
             CREATE users:3 = { last: 'hopper', first: 'grace' };\n\
             CREATE users:4 = { last: 'turing', first: 'alan' };\n\
             CREATE users:5 = { last: 'lovelace', first: 'annabella' };\n\
             CREATE users:6 = { last: 'johnson', first: 'katherine' };",
        )
        .unwrap();
    session
}

const COMPOSITE: &str = "DEFINE INDEX by_name ON users FIELDS last, first;";

fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    let outcomes = session.run(read).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

/// Every read this file cares about, so each test states one thing about all of
/// them rather than each restating the list.
const READS: &[&str] = &[
    "SELECT * FROM users WHERE last = 'lovelace';",
    "SELECT * FROM users WHERE last = 'lovelace' AND first = 'ada';",
    "SELECT * FROM users WHERE last > 'l';",
    "SELECT * FROM users WHERE last >= 'lovelace' AND last < 'm';",
    "SELECT * FROM users WHERE last = 'nobody';",
];

#[test]
fn a_composite_index_serves_its_leading_field() {
    let store = store();
    let mut session = ready(&store);
    session.run(COMPOSITE).unwrap();
    for read in READS {
        assert_eq!(
            plan(&mut session, read, "index"),
            r#"String("by_name")"#,
            "{read}"
        );
    }
    assert_eq!(
        plan(&mut session, READS[0], "shape"),
        r#"String("equality")"#
    );
    assert_eq!(plan(&mut session, READS[2], "shape"), r#"String("range")"#);
}

#[test]
fn the_answers_are_the_ones_a_scan_gives() {
    // The rule the widening could have broken, which is why it is asserted
    // record for record rather than in count: an index changes what a read costs
    // and never what it answers.
    let unindexed = store();
    let mut without = ready(&unindexed);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(COMPOSITE).unwrap();

    for read in READS {
        assert_eq!(ids(&mut with, read), ids(&mut without, read), "{read}");
    }
    // …and the expected answers are written out once, so the equality above is
    // not two wrong answers agreeing.
    assert_eq!(
        ids(&mut with, READS[0]),
        vec![RecordId::Int(1), RecordId::Int(2), RecordId::Int(5)]
    );
    assert_eq!(ids(&mut with, READS[1]), vec![RecordId::Int(1)]);
    assert_eq!(
        ids(&mut with, READS[2]),
        vec![
            RecordId::Int(1),
            RecordId::Int(2),
            RecordId::Int(4),
            RecordId::Int(5)
        ]
    );
    assert!(ids(&mut with, READS[4]).is_empty());
}

#[test]
fn the_second_field_alone_is_still_a_scan() {
    // The boundary. The entries for one `first` are scattered across every
    // `last`, so an index offered for that would be answering about the wrong
    // column.
    let store = store();
    let mut session = ready(&store);
    session.run(COMPOSITE).unwrap();
    let read = "SELECT * FROM users WHERE first = 'ada';";
    assert_eq!(plan(&mut session, read, "access"), r#"String("scan")"#);
    assert_eq!(ids(&mut session, read), vec![RecordId::Int(1)]);
}

#[test]
fn a_unique_composite_promises_no_ceiling() {
    // Uniqueness is over the **pair**, and this condition fixes only the first
    // field — so a lookup can return any number of records. Claiming `at_most 1`
    // would make the planner prefer an index that can return the whole table.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_name ON users FIELDS last, first UNIQUE;")
        .unwrap();
    let read = "SELECT * FROM users WHERE last = 'lovelace';";
    assert_eq!(plan(&mut session, read, "index"), r#"String("by_name")"#);
    assert_eq!(plan(&mut session, read, "at_most"), "None");
    // …and it really does answer with three, which is what the absent ceiling is
    // about.
    assert_eq!(
        ids(&mut session, read),
        vec![RecordId::Int(1), RecordId::Int(2), RecordId::Int(5)]
    );
}

#[test]
fn a_single_field_unique_index_still_promises_one() {
    // The ceiling that is real, unmoved: there the condition fixes every field.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_first ON users FIELDS first UNIQUE;")
        .unwrap();
    let read = "SELECT * FROM users WHERE first = 'ada';";
    assert_eq!(plan(&mut session, read, "at_most"), "Number(Integer(1))");
    assert_eq!(ids(&mut session, read), vec![RecordId::Int(1)]);
}

#[test]
fn a_write_keeps_the_composite_index_in_step() {
    // It was maintained all along; what is new is that somebody reads it. So the
    // maintenance is worth asserting through a read for the first time.
    let store = store();
    let mut session = ready(&store);
    session.run(COMPOSITE).unwrap();
    session.run("UPDATE users:2 SET last = 'turing';").unwrap();
    assert_eq!(
        ids(&mut session, "SELECT * FROM users WHERE last = 'lovelace';"),
        vec![RecordId::Int(1), RecordId::Int(5)],
        "the entry the record left behind still answers"
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM users WHERE last = 'turing';"),
        vec![RecordId::Int(2), RecordId::Int(4)]
    );
    session.run("DELETE users:1;").unwrap();
    assert_eq!(
        ids(&mut session, "SELECT * FROM users WHERE last = 'lovelace';"),
        vec![RecordId::Int(5)]
    );
}

#[test]
fn a_leading_value_that_is_a_prefix_of_another_is_not_confused_with_it() {
    // What the encoding's self-delimiting property buys, stated as a test: the
    // bytes of `'ab'` must not be a byte-prefix of the bytes of `'abc'`, or a
    // lookup for one would answer with the other.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION t;\n\
             CREATE t:1 = { a: 'ab', b: 'x' };\n\
             CREATE t:2 = { a: 'abc', b: 'y' };\n\
             DEFINE INDEX by_ab ON t FIELDS a, b;",
        )
        .unwrap();
    assert_eq!(
        ids(&mut session, "SELECT * FROM t WHERE a = 'ab';"),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM t WHERE a = 'abc';"),
        vec![RecordId::Int(2)]
    );
}
