//! `USING` — the statement says which path it expects, and is refused otherwise.
//!
//! # A refusal, never a router
//!
//! Nothing here reaches the planner. `USING` does not choose a path and cannot
//! make a read faster; it fails the statement when the path taken is not the one
//! named. That is the point: the worst failure mode an indexed store has is the
//! query that quietly stops using its index and starts scanning, and it is worst
//! precisely because nothing goes wrong — the answer is still correct, and the
//! only symptom is a latency graph somebody has to be looking at.
//!
//! # It is checked against what the read did
//!
//! Not against what the planner chose, and the difference is the whole design.
//! An ordered index that cannot fill the statement's bound hands the read to the
//! scan; an assertion satisfied by the planner's *intention* would pass in
//! exactly that case — the one case it exists to catch. The last two tests here
//! pin it from both sides: `USING ordered` is refused on a read that fell back,
//! and `USING scan` is *permitted* on the same read, because the scan is what
//! honestly happened.
//!
//! # Every permit is paired with a refusal
//!
//! A test that only permits passes on an implementation that never refuses
//! anything, which is the same as not having the clause.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The same schema `one_plan` uses, for the same reason: every access path this
/// store has a word for has to be reachable.
const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION users;
DEFINE COLLECTION orders;
DEFINE TABLE follows EDGE;
DEFINE COLLECTION items;
DEFINE INDEX by_email ON users FIELDS email UNIQUE;
DEFINE INDEX by_city ON users FIELDS city;
DEFINE INDEX by_joined ON users FIELDS joined;
DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;
CREATE users:1 = { email: 'ada@example.com', city: 'Paris', joined: 3, name: 'ada' };
CREATE users:2 = { email: 'grace@example.com', city: 'Paris', joined: 2, name: 'grace' };
CREATE users:3 = { email: 'alan@example.com', city: 'Lyon', joined: 1, name: 'alan' };
CREATE orders:1 = { who: 'ada', total: 12 };
RELATE users:1->follows->users:2;
CREATE items:1 = { at: [0.1, 0.2] };
CREATE items:2 = { at: [0.9, 0.8] };
";

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

/// One read per access path, written without an assertion so the path it takes
/// is a fact rather than a request.
const PER_PATH: &[(&str, AccessPath)] = &[
    ("SELECT * FROM users:1", AccessPath::Record),
    (
        "SELECT * FROM users WHERE email = 'ada@example.com'",
        AccessPath::Index,
    ),
    (
        "SELECT * FROM users ORDER BY joined DESC LIMIT 2",
        AccessPath::Ordered,
    ),
    ("SELECT * FROM users", AccessPath::Scan),
    (
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 1 APPROXIMATE",
        AccessPath::Approximate,
    ),
    ("SELECT * FROM users:1->follows->users", AccessPath::Graph),
    (
        "SELECT * FROM users JOIN orders ON users.name = orders.who",
        AccessPath::Join,
    ),
    (
        "SELECT * FROM (SELECT * FROM users LIMIT 2)",
        AccessPath::Materialised,
    ),
];

#[test]
fn each_path_permits_the_word_that_names_it() {
    let store = store();
    let mut session = ready(&store);
    for (read, path) in PER_PATH {
        let script = format!("{read} USING {};", path.name());
        let outcomes = session
            .run(&script)
            .unwrap_or_else(|error| panic!("{script} was refused: {error}"));
        // Permitted *and* still answering — an assertion that swallowed the
        // answer would pass this test and be useless.
        assert_eq!(outcomes.last().unwrap().path(), Some(*path), "{script}");
    }
}

#[test]
fn each_path_refuses_every_word_that_does_not_name_it() {
    let store = store();
    let mut session = ready(&store);
    for (read, path) in PER_PATH {
        for wrong in AccessPath::ALL {
            if wrong == *path {
                continue;
            }
            let script = format!("{read} USING {};", wrong.name());
            match session.run(&script) {
                Err(Error::PathNotTaken { expected, took, .. }) => {
                    assert_eq!(expected, wrong.name(), "{script}");
                    assert_eq!(took, path.name(), "{script}");
                }
                other => panic!("{script} was not refused: {other:?}"),
            }
        }
    }
}

#[test]
fn a_word_that_is_not_a_path_is_refused_before_the_read() {
    let store = store();
    let mut session = ready(&store);
    match session.run("SELECT * FROM users USING inedx;") {
        Err(Error::NoSuchAccessPath { named, known, .. }) => {
            assert_eq!(named, "inedx");
            // The refusal lists the words rather than saying only that this one
            // is wrong, because the author's next move is to write a right one.
            assert!(known.contains("index"), "{known}");
            assert!(known.contains("materialised"), "{known}");
        }
        other => panic!("a typo was not refused: {other:?}"),
    }
}

#[test]
fn the_word_may_be_written_in_any_case() {
    let store = store();
    let mut session = ready(&store);
    session.run("SELECT * FROM users USING SCAN;").unwrap();
    session.run("SELECT * FROM users USING Scan;").unwrap();
}

#[test]
fn an_index_may_be_named_and_a_different_one_refuses() {
    let store = store();
    let mut session = ready(&store);
    const READ: &str = "SELECT * FROM users WHERE email = 'ada@example.com'";

    let outcomes = session
        .run(&format!("{READ} USING INDEX by_email;"))
        .unwrap();
    assert_eq!(outcomes.last().unwrap().records().unwrap().len(), 1);

    // The question `USING index` cannot ask. Both indexes exist and both are on
    // `users`; only one of them served this read, and a plan regression that
    // swapped them is invisible to the path word.
    match session.run(&format!("{READ} USING INDEX by_city;")) {
        Err(Error::IndexNotUsed { expected, took, .. }) => {
            assert_eq!(expected, "by_city");
            assert_eq!(took, "`by_email`");
        }
        other => panic!("the wrong index was not refused: {other:?}"),
    }
}

#[test]
fn naming_an_index_on_a_read_that_used_none_says_so() {
    let store = store();
    let mut session = ready(&store);
    match session.run("SELECT * FROM users USING INDEX by_city;") {
        Err(Error::IndexNotUsed { expected, took, .. }) => {
            assert_eq!(expected, "by_city");
            assert_eq!(took, "no index", "a scan reported an index");
        }
        other => panic!("a scan was not refused: {other:?}"),
    }
}

#[test]
fn a_read_that_fell_back_is_refused_for_the_path_it_did_not_take() {
    let store = store();
    let mut session = thin(&store);
    // The planner chooses `ordered` here and the read cannot fill the bound from
    // the index, so it scans. This is the case the whole clause exists for, and
    // an assertion checked against the planner's choice would permit it.
    match session.run(&format!("{THIN} USING ordered;")) {
        Err(Error::PathNotTaken { expected, took, .. }) => {
            assert_eq!(expected, "ordered");
            assert_eq!(took, "scan");
        }
        other => panic!("a fallback was not caught: {other:?}"),
    }
}

#[test]
fn the_same_read_permits_the_path_it_actually_took() {
    let store = store();
    let mut session = thin(&store);
    // The other half, and not a formality: a clause that refused everything
    // would pass the test above. `scan` is what honestly happened, so `scan` is
    // permitted — and the answer is still the right one.
    let outcomes = session.run(&format!("{THIN} USING scan;")).unwrap();
    assert_eq!(outcomes.last().unwrap().records().unwrap().len(), 10);
}

#[test]
fn an_assertion_inside_a_materialised_source_is_the_inner_read_s() {
    let store = store();
    let mut session = ready(&store);
    // The inner read picks its own path and states its own expectation. The
    // outer statement is `materialised` whatever the inner one did, so the two
    // assertions are about different reads and both hold at once.
    session
        .run(
            "SELECT * FROM (SELECT * FROM users WHERE email = 'ada@example.com' \
             LIMIT 1 USING index) USING materialised;",
        )
        .unwrap();
    // And the inner one is really checked, rather than parsed and dropped.
    match session.run(
        "SELECT * FROM (SELECT * FROM users WHERE email = 'ada@example.com' \
         LIMIT 1 USING scan) USING materialised;",
    ) {
        Err(Error::PathNotTaken { expected, took, .. }) => {
            assert_eq!(expected, "scan");
            assert_eq!(took, "index");
        }
        other => panic!("an inner assertion was not checked: {other:?}"),
    }
}

#[test]
fn every_path_has_a_word_and_every_word_a_path() {
    // The guard on `ALL`, which Rust cannot check: a variant added without a row
    // there is unassertable by `USING` and missing from the refusal that lists
    // the words, and neither failure raises anything on its own.
    assert_eq!(AccessPath::ALL.len(), 8);
    for path in AccessPath::ALL {
        assert_eq!(AccessPath::named(path.name()), Some(path));
        assert!(AccessPath::known().contains(path.name()));
    }
    assert_eq!(AccessPath::named("nothing"), None);
}

const THIN: &str = "SELECT * FROM events WHERE rare = 0 ORDER BY at DESC LIMIT 10";

/// A table whose condition is too thin for the order its index holds.
fn thin(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION events;\n\
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
