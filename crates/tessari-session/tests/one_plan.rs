//! One plan structure, reported by `EXPLAIN` and by the answer.
//!
//! # What was wrong before this
//!
//! There were two structures describing the same read, and they spoke different
//! words. A traversal explained as `graph` and answered `index`; a join
//! explained as `join` and answered `index` or `scan` depending on which side
//! got probed; a materialised source explained as `materialised` and answered
//! whatever the *inner* read had done, which said this statement used an index
//! when it read from a held vector. None of that was a wrong answer, and
//! together it made the plan unusable: comparing what a statement said it would
//! do against what it did meant translating between two vocabularies, so nobody
//! did, so a plan that had drifted from the read would not have been noticed.
//!
//! So there is one type, one renderer, and — for the choice that carries the
//! index name, the shape, the columns and the ceiling — one function that fills
//! it, called by the read that runs the choice and by the `EXPLAIN` that only
//! describes it.
//!
//! # The one place they are allowed to differ, and why that is the feature
//!
//! The planner cannot know whether an ordered index will fill the statement's
//! bound: that question *is* the read. So a read whose index runs out reports
//! the scan it settled for while `EXPLAIN` reports the order it chose, and the
//! difference is the honest answer rather than a bug. The last test here pins
//! that case and asserts the note that names it, because an unexplained
//! disagreement and an explained one look identical from the outside.
//!
//! Making them agree by running the read inside `EXPLAIN` was considered and
//! rejected: it buys this file one more passing row and costs `EXPLAIN` the
//! property that makes it worth having, which is that it is cheap.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// Enough schema to reach every access path the store has a word for.
const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE TABLE users;
DEFINE TABLE orders;
DEFINE TABLE follows EDGE;
DEFINE TABLE stops;
DEFINE TABLE items;
DEFINE INDEX by_email ON users FIELDS email UNIQUE;
DEFINE INDEX by_city ON users FIELDS city;
DEFINE INDEX by_joined ON users FIELDS joined;
DEFINE INDEX by_where ON stops FIELDS at SPATIAL;
DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;
CREATE users:1 = { email: 'ada@example.com', city: 'Paris', joined: 3, name: 'ada' };
CREATE users:2 = { email: 'grace@example.com', city: 'Paris', joined: 2, name: 'grace' };
CREATE users:3 = { email: 'alan@example.com', city: 'Lyon', joined: 1, name: 'alan' };
CREATE orders:1 = { who: 'ada', total: 12 };
CREATE orders:2 = { who: 'alan', total: 4 };
RELATE users:1->follows->users:2;
CREATE stops:'gate' = { at: geometry { type: 'Point', coordinates: [2.35, 48.85] } };
CREATE stops:'mill' = { at: geometry { type: 'Point', coordinates: [2.09, 48.93] } };
CREATE items:1 = { at: [0.1, 0.2] };
CREATE items:2 = { at: [0.9, 0.8] };
";

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

/// The plan `EXPLAIN` reports for a read, as the value it answers with.
fn explained(session: &mut Session<'_>, read: &str) -> Value {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(plan)) = outcomes.last() else {
        panic!("an EXPLAIN answered with {:?}", outcomes.last());
    };
    plan.clone()
}

/// The plan the read itself carried, rendered the same way, with its notes.
fn answered(session: &mut Session<'_>, read: &str) -> (Value, Vec<Note>) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { plan, notes, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (plan.to_value(), notes.clone())
}

/// Every shape of read the store has an access path for, except the one that
/// falls back — which has its own test below.
///
/// Each row carries the word it is *expected* to answer under, and the test
/// asserts it. Without that the list is a claim about coverage rather than a
/// check on it: two rows here quietly degraded to `scan` when they were first
/// written — the vector read for want of `APPROXIMATE`, and both sides agreed on
/// `scan`, so the equality passed while covering nothing.
const SHAPES: &[(&str, &str)] = &[
    // A table with nothing to narrow it.
    ("SELECT * FROM users;", "scan"),
    // One record by identity.
    ("SELECT * FROM users:1;", "record"),
    // This node, out of `meta` — no table, no index, no choice.
    ("SELECT * FROM $node;", "record"),
    // A unique index answering an equality, which is the one candidate that can
    // promise a ceiling for free.
    (
        "SELECT * FROM users WHERE email = 'ada@example.com';",
        "index",
    ),
    // A secondary index answering an equality, which cannot.
    ("SELECT * FROM users WHERE city = 'Paris';", "index"),
    // A range, over the same index.
    ("SELECT * FROM users WHERE joined > 1;", "index"),
    // A condition no index serves.
    ("SELECT * FROM users WHERE name = 'ada';", "scan"),
    // An order an index holds, with a bound it can fill.
    (
        "SELECT * FROM users ORDER BY joined DESC LIMIT 2;",
        "ordered",
    ),
    // A walk, which reads an index per step and chooses none of them.
    ("SELECT * FROM users:1->follows->users;", "graph"),
    // Two reads brought together on a key.
    (
        "SELECT * FROM users JOIN orders ON users.name = orders.who;",
        "join",
    ),
    // A read standing where a table stands.
    (
        "SELECT * FROM (SELECT * FROM users LIMIT 2);",
        "materialised",
    ),
    // A spatial index walked nearest-first — exact, and its own shape.
    (
        "SELECT * FROM stops ORDER BY geo::distance(at, \
         geometry { type: 'Point', coordinates: [2.3, 48.8] }) LIMIT 1;",
        "ordered",
    ),
    // A vector index, the one read an index answers differently from a scan.
    // `APPROXIMATE` is what admits it: without the word the read is exact and
    // this row would silently be another scan.
    (
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 1 APPROXIMATE;",
        "approximate",
    ),
];

#[test]
fn every_shape_of_read_explains_as_it_answers() {
    let store = store();
    let mut session = ready(&store);
    for (read, word) in SHAPES {
        let plan = explained(&mut session, read);
        let (took, notes) = answered(&mut session, read);
        assert_eq!(plan, took, "{read}");
        // The row covers the path it says it covers, so the equality above is a
        // check on thirteen shapes rather than on however many of them happened
        // to fall through to the same one.
        let Value::Object(fields) = &took else {
            panic!("a plan rendered as {took:?}");
        };
        assert_eq!(fields.get("access"), Some(&Value::from(*word)), "{read}");
        // None of these shapes falls back, so the agreement above is the whole
        // story rather than a difference the notes were covering for.
        assert!(
            !notes
                .iter()
                .any(|note| matches!(note, Note::FellBack { .. })),
            "{read} fell back: {notes:?}"
        );
    }
}

#[test]
fn the_plan_names_the_index_that_served_the_read() {
    let store = store();
    let mut session = ready(&store);
    // Not a tautology against the test above: two identical *empty* plans would
    // satisfy equality, so at least one shape has to be checked for content.
    let (took, _) = answered(
        &mut session,
        "SELECT * FROM users WHERE email = 'ada@example.com';",
    );
    let Value::Object(fields) = took else {
        panic!("a plan rendered as {took:?}");
    };
    assert_eq!(fields.get("access"), Some(&Value::from("index")));
    assert_eq!(fields.get("table"), Some(&Value::from("users")));
    assert_eq!(fields.get("index"), Some(&Value::from("by_email")));
    assert_eq!(fields.get("shape"), Some(&Value::from("equality")));
    assert_eq!(
        fields.get("columns").map(ToString::to_string),
        Some("1".to_owned())
    );
    assert_eq!(
        fields.get("at_most").map(ToString::to_string),
        Some("1".to_owned())
    );
}

#[test]
fn a_plan_reports_only_what_this_read_knew() {
    let store = store();
    let mut session = ready(&store);
    let (took, _) = answered(&mut session, "SELECT * FROM users;");
    let Value::Object(fields) = took else {
        panic!("a plan rendered as {took:?}");
    };
    // A scan chose no index, so there is no index key rather than an index key
    // saying nothing. Eight fields of which six are null describe a read worse
    // than two fields do.
    assert_eq!(fields.get("access"), Some(&Value::from("scan")));
    assert_eq!(fields.get("table"), Some(&Value::from("users")));
    assert_eq!(fields.get("index"), None);
    assert_eq!(fields.get("shape"), None);
    assert_eq!(fields.get("at_most"), None);
}

#[test]
fn a_read_that_fell_back_reports_what_it_did_and_says_why() {
    let store = store();
    let mut session = store_with_a_thin_condition(&store);
    // The condition matches one record in a hundred over the order, so filling
    // a bound of ten needs about a thousand entries — past the walk's reach.
    const THIN: &str = "SELECT * FROM events WHERE rare = 0 ORDER BY at DESC LIMIT 10;";
    let plan = explained(&mut session, THIN);
    let (took, notes) = answered(&mut session, THIN);

    // The one disagreement the design admits, and the reason it is admitted:
    // whether the index fills the bound is the read, so the planner cannot know
    // it and reports the order it chose.
    assert_ne!(plan, took);
    let Value::Object(chose) = plan else {
        panic!("a plan rendered wrongly");
    };
    let Value::Object(did) = took else {
        panic!("a plan rendered wrongly");
    };
    assert_eq!(chose.get("access"), Some(&Value::from("ordered")));
    assert_eq!(did.get("access"), Some(&Value::from("scan")));

    // And the answer carries the note that makes the difference readable
    // instead of a discrepancy somebody has to explain to themselves.
    assert_eq!(
        notes,
        vec![Note::FellBack {
            from: AccessPath::Ordered,
            to: AccessPath::Scan,
        }]
    );
}

/// A table whose condition is too thin for the order its index holds.
fn store_with_a_thin_condition(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events;\n\
             DEFINE INDEX by_at ON events FIELDS at;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 1_u32..=1000 {
        script.push_str(&format!(
            "CREATE events:{n} = {{ at: {}, rare: {} }};\n",
            n / 2,
            n % 100
        ));
        if n % 200 == 0 {
            session.run(&script).unwrap();
            script.clear();
        }
    }
    session
}
