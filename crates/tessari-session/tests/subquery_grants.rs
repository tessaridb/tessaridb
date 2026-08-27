//! A grant covers every table a statement can reach, including the ones a
//! **subquery** names.
//!
//! # What these tests are a regression for
//!
//! A read used to be checked against the tables its `FROM` named, and nothing
//! else. A projection, a `WHERE`, an `ORDER BY` and a `GROUP BY` are all
//! expressions, and an expression may hold a read — so
//! `SELECT (SELECT pay FROM salaries) AS leaked FROM public` named `public` to
//! the grant loop and answered with `salaries`. There was no error and no
//! warning: the check ran against a list the second table was never on, and a
//! caller granted `read` on one harmless table was handed the contents of every
//! other table in the database.
//!
//! Written as its own file rather than folded into the grant suite, because
//! what it pins is not "grants work" but "a grant is asked about every table a
//! statement can reach, wherever in the statement the name was written". Each
//! test below is one such place.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery staple";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// One table the caller may read and one they may not.
fn ready(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE public; CREATE public:1 = { n: 1 };\n\
             DEFINE TABLE salaries; CREATE salaries:1 = { pay: 999 };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery staple';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE USER spy ON prod.shop ROLE viewer PASSWORD 'correct horse battery staple';\n\
         GRANT read ON public TO spy;\n\
         DEFINE USER scribe ON prod.shop ROLE editor PASSWORD 'correct horse battery staple';\n\
         GRANT read, write ON public TO scribe;",
    )
    .unwrap();
}

fn spy(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.sign_in("spy", PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// Every statement below must be refused, and refused by **name** — an empty
/// answer would look identical to a table that happens to hold nothing, which
/// is exactly how this went unnoticed.
fn refused(store: &Store, script: &str) {
    refused_as(store, "spy", script);
}

/// The same, as a named user — a write needs the role that may write, or the
/// role check refuses first and the grant this file is about is never reached.
fn refused_as(store: &Store, who: &str, script: &str) {
    let mut session = Session::new(store);
    session.sign_in(who, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let failed = session.run(script);
    let message = match failed {
        Err(failure) => format!("{failure}"),
        Ok(answered) => panic!("{script}\nwas allowed, and answered {answered:?}"),
    };
    assert!(
        message.contains("salaries"),
        "{script}\nwas refused without naming the table: {message}"
    );
}

#[test]
fn the_ungranted_table_is_refused_when_it_is_the_source() {
    let store = store();
    ready(&store);
    refused(&store, "SELECT * FROM salaries;");
}

#[test]
fn and_when_it_is_read_from_a_projection() {
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT (SELECT pay FROM salaries) AS leaked FROM public;",
    );
}

#[test]
fn and_from_a_filter() {
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT * FROM public WHERE n IN (SELECT pay FROM salaries);",
    );
}

#[test]
fn and_from_an_ordering() {
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT * FROM public ORDER BY (SELECT pay FROM salaries);",
    );
}

#[test]
fn and_from_a_grouping() {
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT count(*) AS n FROM public GROUP BY (SELECT pay FROM salaries);",
    );
}

#[test]
fn and_from_inside_a_function_argument() {
    // Nested a level deeper than any of the above, because a walk that stopped
    // at the top of the expression would let this one through.
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT array::len((SELECT pay FROM salaries)) AS n FROM public;",
    );
}

#[test]
fn and_from_a_written_value() {
    // A write can copy what a read may not see. `public` is granted `read`
    // only, so this is refused twice over — the assertion is that `salaries` is
    // the table named, which is the half that was missing.
    let store = store();
    ready(&store);
    refused_as(
        &store,
        "scribe",
        "CREATE public:2 = { copy: (SELECT pay FROM salaries) };",
    );
}

#[test]
fn and_from_a_binding() {
    let store = store();
    ready(&store);
    refused(&store, "LET $all = SELECT pay FROM salaries; RETURN $all;");
}

#[test]
fn and_from_the_scripts_answer() {
    let store = store();
    ready(&store);
    refused(&store, "RETURN (SELECT pay FROM salaries);");
}

#[test]
fn and_from_a_materialised_source() {
    // A `FROM` may now name a read rather than a table, which is a second place
    // a table name can be written and a second way the grant loop could have
    // been handed a list the table was never on.
    let store = store();
    ready(&store);
    refused(&store, "SELECT * FROM (SELECT pay FROM salaries LIMIT 10);");
}

#[test]
fn and_from_the_condition_over_one() {
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT * FROM (SELECT * FROM public LIMIT 10) \
         WHERE n = (SELECT pay FROM salaries);",
    );
}

#[test]
fn and_from_either_side_of_a_join() {
    let store = store();
    ready(&store);
    refused(
        &store,
        "SELECT * FROM (SELECT pay FROM salaries LIMIT 10) AS s \
         JOIN public AS p ON s.pay = p.n;",
    );
    refused(
        &store,
        "SELECT * FROM public AS p \
         JOIN (SELECT pay FROM salaries LIMIT 10) AS s ON p.n = s.pay;",
    );
}

#[test]
fn the_granted_table_still_reads() {
    // The other half: closing the hole must not close the door.
    let store = store();
    ready(&store);
    let mut session = spy(&store);
    let outcomes = session.run("SELECT * FROM public;").unwrap();
    assert_eq!(outcomes[0].records().map(<[_]>::len), Some(1));
}
