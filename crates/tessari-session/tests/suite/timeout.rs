//! `TIMEOUT` — a read that passes its ceiling is refused, not truncated.
//!
//! # The one property worth testing hardest
//!
//! Not that a slow read stops. That a read which stops **answers nothing**. The
//! records are already in hand when the ceiling passes, and handing them back is
//! free and looks like success — a caller counting them, summing them or writing
//! them somewhere would be wrong and would have no way to find out. So every
//! refusal here is asserted to be a refusal, and the permits beside them are
//! asserted to answer in full, because a clause that refused everything would
//! pass a file of refusal tests.
//!
//! # Why the ceilings are extreme
//!
//! `1ns` has passed before the first record is produced, and `1h` will not pass
//! during a test. Neither depends on how fast the machine running this is, which
//! a ceiling in the middle would — and a timing test that fails on a loaded
//! laptop teaches a team to re-run the suite until it is green.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const RECORDS: usize = 200;

/// A table with enough records that a read of it produces more than one.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION events;\n\
             DEFINE INDEX by_at ON events FIELDS at;\n\
             DEFINE INDEX timeout ON events FIELDS rare;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 1..=RECORDS {
        script.push_str(&format!(
            "CREATE events:{n} = {{ at: {n}, rare: {} }};\n",
            n % 10
        ));
    }
    session.run(&script).unwrap();
    session
}

/// Reads covering both consumers a read can run through, since the ceiling is
/// spent in the consumer and there are two of them.
///
/// The first streams — every record is produced, shaped and offered to a bound.
/// The second holds: a grouping is a barrier that must see the whole set before
/// it may emit anything, so it collects. A ceiling that bit on only one of them
/// would be a ceiling that half the language's reads ignore.
const BOTH_CONSUMERS: &[&str] = &[
    "SELECT * FROM events ORDER BY at DESC LIMIT 10",
    "SELECT rare, count(*) AS n FROM events GROUP BY rare",
];

#[test]
fn a_read_inside_its_ceiling_answers_in_full() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT * FROM events TIMEOUT 1h;")
        .unwrap_or_else(|error| panic!("a generous ceiling refused a read: {error}"));
    assert_eq!(outcomes.last().unwrap().records().unwrap().len(), RECORDS);
}

#[test]
fn a_read_that_passes_its_ceiling_is_refused() {
    let store = store();
    let mut session = ready(&store);
    match session.run("SELECT * FROM events TIMEOUT 1ns;") {
        Err(Error::TimedOut {
            after, produced, ..
        }) => {
            // The ceiling is quoted back as it was written, so the refusal is
            // legible beside the statement that caused it.
            assert_eq!(after, "1ns");
            // How far it got — the one thing a truncated answer would have told
            // the caller, in the one place a caller cannot mistake for a result.
            assert!(produced >= 1, "the refusal claimed no records");
        }
        other => panic!("a passed ceiling did not refuse: {other:?}"),
    }
}

#[test]
fn a_refused_read_answers_nothing_at_all() {
    let store = store();
    let mut session = ready(&store);
    // The whole point. The records exist, they are correct, and returning them
    // would cost nothing — which is exactly why a truncated answer is the easy
    // mistake here and why this asserts there is no answer rather than a short
    // one.
    let refused = session.run("SELECT * FROM events TIMEOUT 1ns;");
    assert!(refused.is_err(), "a passed ceiling answered");
    // And the session is still usable afterwards: a refusal is one statement's
    // failure, not the connection's.
    let after = session.run("SELECT * FROM events;").unwrap();
    assert_eq!(after.last().unwrap().records().unwrap().len(), RECORDS);
}

#[test]
fn every_consumer_a_read_can_run_through_honours_the_ceiling() {
    let store = store();
    let mut session = ready(&store);
    for read in BOTH_CONSUMERS {
        // Refused under a ceiling that has already passed …
        match session.run(&format!("{read} TIMEOUT 1ns;")) {
            Err(Error::TimedOut { .. }) => {}
            other => panic!("{read} ignored its ceiling: {other:?}"),
        }
        // … and answering under one that has not. Without this half, a consumer
        // that refused unconditionally would pass.
        session
            .run(&format!("{read} TIMEOUT 1h;"))
            .unwrap_or_else(|error| panic!("{read} was refused under 1h: {error}"));
    }
}

#[test]
fn a_ceiling_that_could_only_refuse_is_caught_when_the_statement_is_read() {
    let store = store();
    let mut session = ready(&store);
    // Refused before anything runs, rather than after a scan the clause could
    // never have permitted. A clause that can only refuse is a mistake in the
    // statement, and a mistake in a statement is a thing to say when the
    // statement is read.
    match session.run("SELECT * FROM events TIMEOUT 0s;") {
        Err(Error::Script(tessari_ql::Error::EmptyTimeout { written, .. })) => {
            assert_eq!(written, "0s");
        }
        other => panic!("a zero ceiling was accepted: {other:?}"),
    }
}

#[test]
fn an_inner_ceiling_narrows_and_never_widens() {
    let store = store();
    let mut session = ready(&store);
    // The inner read is the tight one: its own ceiling refuses it, and the
    // statement holding it fails with it.
    match session.run("SELECT * FROM (SELECT * FROM events LIMIT 200 TIMEOUT 1ns);") {
        Err(Error::TimedOut { .. }) => {}
        other => panic!("an inner ceiling was ignored: {other:?}"),
    }
    // The outer read is the tight one, and the inner read's generous ceiling
    // does not lift it. This is the direction that matters: a subquery able to
    // raise the budget its caller set would make an outer ceiling a suggestion.
    match session.run("SELECT * FROM (SELECT * FROM events LIMIT 200 TIMEOUT 1h) TIMEOUT 1ns;") {
        Err(Error::TimedOut { .. }) => {}
        other => panic!("an inner clause widened an outer ceiling: {other:?}"),
    }
    // And two generous ceilings still answer, so the test above is about
    // narrowing rather than about nesting refusing everything.
    let outcomes = session
        .run("SELECT * FROM (SELECT * FROM events LIMIT 200 TIMEOUT 1h) TIMEOUT 1h;")
        .unwrap();
    assert_eq!(outcomes.last().unwrap().records().unwrap().len(), RECORDS);
}

#[test]
fn a_read_with_no_ceiling_is_never_refused_for_time() {
    let store = store();
    let mut session = ready(&store);
    // The ordinary read, which is nearly every read. It pays one branch on
    // `None` per record and can never fail this way.
    let outcomes = session.run("SELECT * FROM events;").unwrap();
    assert_eq!(outcomes.last().unwrap().records().unwrap().len(), RECORDS);
}

#[test]
fn a_ceiling_and_an_assertion_are_independent_clauses() {
    let store = store();
    let mut session = ready(&store);
    // `USING` and `TIMEOUT` sit in the same statement tail and are checked at
    // different moments — the ceiling as records are produced, the assertion on
    // the plan the read finished with. Written together they still mean what
    // each means alone.
    let outcomes = session
        .run("SELECT * FROM events USING scan TIMEOUT 1h;")
        .unwrap();
    assert_eq!(outcomes.last().unwrap().records().unwrap().len(), RECORDS);
    // The ceiling passes first, so the read never reaches the assertion — and
    // the refusal names the ceiling rather than the path.
    match session.run("SELECT * FROM events USING index TIMEOUT 1ns;") {
        Err(Error::TimedOut { .. }) => {}
        other => panic!("a ceiling and an assertion did not compose: {other:?}"),
    }
}

#[test]
fn an_index_may_still_be_called_timeout() {
    let store = store();
    let mut session = ready(&store);
    // The ambiguity the clause created and the reason it is settled by lookahead
    // rather than by reserving the word: both of these are sayable, and which
    // one is meant is decided by whether a duration follows.
    let named = session
        .run("SELECT * FROM events WHERE rare = 3 USING INDEX timeout;")
        .unwrap_or_else(|error| panic!("an index called `timeout` was unnameable: {error}"));
    assert_eq!(named.last().unwrap().records().unwrap().len(), RECORDS / 10);

    let clause = session
        .run("SELECT * FROM events USING index TIMEOUT 1h;")
        .unwrap_err();
    // The read scanned, so the assertion refuses — which is the point: the
    // statement was read as `USING index` plus a ceiling, not as an index named
    // `timeout`, and the refusal names the path rather than a syntax error.
    assert!(
        matches!(clause, Error::PathNotTaken { .. }),
        "the tail was misread: {clause:?}"
    );
}
