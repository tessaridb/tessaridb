//! `DEFINE SERIES` — the statement, and what it declares.
//!
//! The floor itself is asserted in `tessari-storage`'s own suite, where a test
//! can read the substrate and prove the records below it are still there. What
//! is asserted here is the language half: that the statement declares what it
//! says, that `INFO` writes back the statement that created it, that a
//! retention which could only empty the answer is refused where it is written,
//! and that the word means what it says when it removes something.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE metrics; USE DATABASE metrics;",
        )
        .unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

fn refused(session: &mut Session<'_>, script: &str) -> String {
    match session.run(script) {
        Err(why) => why.to_string(),
        Ok(outcome) => panic!("expected a refusal, got {outcome:?}"),
    }
}

#[test]
fn a_series_is_declared_and_written_back_as_the_statement_that_made_it() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 12h;");

    let described = format!("{:?}", run(&mut session, "INFO FOR TABLE readings;"));
    assert!(
        described.contains("DEFINE SERIES readings RETAIN 12h"),
        "INFO answered with {described}"
    );
}

#[test]
fn a_retention_in_days_is_written_back_in_hours() {
    // Not a defect introduced here: `Duration::to_literal` normalises, and a
    // queue's timeout has always round-tripped the same way. Asserted rather
    // than left to be discovered, because `RETAIN 30d` is the spelling somebody
    // will actually write and `INFO` answering `720h` is a surprise worth
    // pinning to a test that says it is deliberate.
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 30d;");

    let described = format!("{:?}", run(&mut session, "INFO FOR TABLE readings;"));
    assert!(
        described.contains("DEFINE SERIES readings RETAIN 720h"),
        "INFO answered with {described}"
    );
}

#[test]
fn a_series_answers_with_a_record_written_now() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE SERIES readings RETAIN 30d;");
    run(&mut session, "CREATE readings = { celsius: 21 };");

    // The floor is thirty days back and the record is a moment old, so the
    // engine's ordinary case is that nothing is hidden at all.
    let answered = run(&mut session, "SELECT * FROM readings;");
    let Outcome::Records { records, .. } = answered else {
        panic!("a read answers with records");
    };
    assert_eq!(records.len(), 1);
}

#[test]
fn a_retention_that_could_only_empty_the_answer_is_refused_where_it_is_written() {
    let store = store();
    let mut session = opened(&store);

    for written in ["RETAIN 0s", "RETAIN -1d"] {
        let why = refused(&mut session, &format!("DEFINE SERIES readings {written};"));
        assert!(
            why.contains("leaves nothing to answer with"),
            "{written} was refused with {why}"
        );
    }
}

#[test]
fn a_series_needs_its_retention() {
    let store = store();
    let mut session = opened(&store);

    let why = refused(&mut session, "DEFINE SERIES readings;");
    assert!(why.contains("RETAIN"), "refused with {why}");
}

#[test]
fn dropping_a_series_that_is_a_plain_table_is_refused() {
    let store = store();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE readings SCHEMALESS;");

    // The word in the statement is a claim about what is being removed. A
    // `DROP SERIES` that removed a plain table would be a statement doing
    // something other than what it says.
    let why = refused(&mut session, "DROP SERIES readings;");
    assert!(why.contains("readings"), "refused with {why}");

    // And the table is still there, which is what makes the refusal a refusal
    // rather than a message printed after the fact.
    run(&mut session, "SELECT * FROM readings;");
}
