//! The ceiling on a read standing in an expression.
//!
//! # The failure
//!
//! A read another statement holds is built whole in memory before that statement
//! asks anything of it, so an unbounded one is an unbounded allocation inside a
//! single statement. Where such a read stands as a **source** the grammar
//! already refuses it without a `LIMIT` — `FROM (SELECT …)` will not parse
//! bare. An **expression** is the position that rule does not reach:
//! `WHERE n IN (SELECT n FROM events)` parses, runs, and builds the table into
//! an array.
//!
//! # Why a refusal and not a note
//!
//! Because a note is impossible here and would be wrong if it were not. The
//! answer in this position is a value, and a value has no room beside it to say
//! anything. And unbounded, this read is expensive and **right**; a default that
//! quietly kept the first ten thousand would be cheap and wrong, and a `count`
//! over the prefix would be a confident number nobody could tell from the true
//! one.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// The ceiling this node applies to a read that named none.
///
/// Written out rather than imported: it is not this crate's public vocabulary,
/// and a test that has to be edited when the ceiling moves is the point.
const CEILING: usize = 10_000;

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE TABLE events;
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A session over `events` holding `count` records, ids `1..=count`.
fn holding(store: &Store, count: usize) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    let mut script = String::new();
    for n in 1..=count {
        writeln!(script, "CREATE events:{n} = {{ n: {n} }};").unwrap();
    }
    session.run(&script).unwrap();
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

#[test]
fn a_read_in_a_condition_that_named_no_bound_is_refused() {
    let store = store();
    let mut session = holding(&store, CEILING + 1);
    let said = refusal(
        &mut session,
        "SELECT * FROM events WHERE n IN (SELECT n FROM events);",
    );
    assert!(said.contains("without a bound of its own"), "{said}");
    // The refusal names the word that lifts it. A ceiling a caller cannot get
    // out from under is a limit on the language, not a bound on this read.
    assert!(said.contains("LIMIT"), "{said}");
}

#[test]
fn a_bound_read_in_an_expression_answers() {
    let store = store();
    let mut session = holding(&store, CEILING + 1);
    // The escape is one word, and it is the same word the source position
    // demands at parse time — one rule for the author even though it is
    // enforced in two places for two different reasons.
    let answered = run(&mut session, "RETURN (SELECT n FROM events LIMIT 3);");
    let Outcome::Value(Value::Array(held)) = answered else {
        panic!("a bound read in an expression did not answer with an array")
    };
    assert_eq!(held.len(), 3);
}

#[test]
fn a_read_bound_to_a_name_that_named_no_bound_is_refused() {
    let store = store();
    let mut session = holding(&store, CEILING + 1);
    // `LET` is the other way into this position, and it is the one where the
    // array is held for the length of the script rather than one condition.
    let said = refusal(&mut session, "LET $all = (SELECT n FROM events);");
    assert!(said.contains("without a bound of its own"), "{said}");
}

#[test]
fn a_read_at_the_ceiling_answers() {
    let store = store();
    let mut session = holding(&store, CEILING);
    // The boundary in the direction nobody tests: a ceiling of ten thousand
    // that refuses *at* ten thousand is a ceiling of nine thousand nine hundred
    // and ninety-nine, and the message would be lying about which.
    let answered = run(&mut session, "LET $all = (SELECT n FROM events); RETURN 1;");
    assert!(matches!(answered, Outcome::Value(_)), "{answered:?}");
}

#[test]
fn a_top_level_read_is_held_for_nobody_and_is_not_refused() {
    let store = store();
    let mut session = holding(&store, CEILING + 1);
    // The caller asked for these records and gets them. This ceiling is about a
    // read built into memory *for another statement*, which is a different
    // thing from a large answer somebody asked for and can see the size of.
    let answered = run(&mut session, "SELECT * FROM events;");
    assert_eq!(answered.records().unwrap().len(), CEILING + 1);
}

#[test]
fn a_scalar_read_is_never_troubled_by_this() {
    let store = store();
    let mut session = holding(&store, CEILING + 1);
    // The reason this position gets a runtime ceiling rather than the source
    // position's parse-time demand: a read here is usually one value, and
    // requiring `LIMIT 1` of every one of them would be noise around the common
    // case in order to bound the rare one. A grouping folds before it holds.
    let answered = run(
        &mut session,
        "LET $n = (SELECT count(*) AS c FROM events); RETURN $n;",
    );
    assert!(matches!(answered, Outcome::Value(_)), "{answered:?}");
}
