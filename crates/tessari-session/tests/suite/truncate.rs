//! `TRUNCATE TABLE t` (ADR-0134 D9): every record removed, the definition kept.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Three orders under a declared table with an index and an event that logs
/// every delete.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders (who string REQUIRED, total int);\n\
             DEFINE INDEX by_who ON orders FIELDS who;\n\
             DEFINE COLLECTION log;\n\
             DEFINE EVENT audit ON orders FOR DELETE THEN CREATE log = { gone: $id };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };\n\
             CREATE orders:2 = { who: 'ada', total: 7 };\n\
             CREATE orders:3 = { who: 'grace', total: 1 };",
        )
        .unwrap();
    session
}

fn count(session: &mut Session<'_>, statement: &str) -> usize {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("records, got {other:?}"),
    }
}

fn removed(session: &mut Session<'_>, statement: &str) -> u64 {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Removed { count } => count,
        other => panic!("a count, got {other:?}"),
    }
}

#[test]
fn every_record_goes_and_the_answer_says_how_many() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(removed(&mut session, "TRUNCATE TABLE orders;"), 3);
    assert_eq!(count(&mut session, "SELECT * FROM orders;"), 0);
    assert_eq!(removed(&mut session, "TRUNCATE orders;"), 0);
}

#[test]
fn the_definition_stays_and_still_decides() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("TRUNCATE orders;").unwrap();

    // Still a declared table: an undeclared field is refused, a declared one lands.
    let refused = session
        .run_now("CREATE orders:9 = { who: 'alan', colour: 'red' };")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("colour"), "{refused}");
    session
        .run_now("CREATE orders:9 = { who: 'alan', total: 2 };")
        .unwrap();
    assert_eq!(count(&mut session, "SELECT * FROM orders;"), 1);
}

#[test]
fn the_index_is_emptied_with_the_records() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("TRUNCATE orders;").unwrap();

    // Served by the index, which must hold no entry for a record that is gone.
    assert_eq!(
        count(
            &mut session,
            "SELECT * FROM orders WHERE who = 'ada' USING INDEX by_who;"
        ),
        0
    );
    session
        .run_now("CREATE orders:4 = { who: 'ada', total: 5 };")
        .unwrap();
    assert_eq!(
        count(
            &mut session,
            "SELECT * FROM orders WHERE who = 'ada' USING INDEX by_who;"
        ),
        1
    );
}

#[test]
fn no_event_runs_for_the_records_it_removes() {
    let store = store();
    let mut session = ready(&store);

    session.run_now("TRUNCATE orders;").unwrap();
    assert_eq!(count(&mut session, "SELECT * FROM log;"), 0);

    // The event is still declared: a delete still runs it.
    session
        .run_now("CREATE orders:5 = { who: 'ada' }; DELETE orders:5;")
        .unwrap();
    assert_eq!(count(&mut session, "SELECT * FROM log;"), 1);
}

#[test]
fn a_topic_is_trimmed_rather_than_truncated() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE TOPIC news;").unwrap();

    let refused = session.run_now("TRUNCATE news;").unwrap_err().to_string();

    assert!(
        refused.contains("topic `news` cannot be truncated"),
        "{refused}"
    );
}

#[test]
fn it_is_one_transaction_with_what_runs_beside_it() {
    let store = store();
    let mut session = ready(&store);

    let refused = session.run_now("BEGIN; TRUNCATE orders; THROW 'no'; COMMIT;");
    assert!(refused.is_err());
    assert_eq!(count(&mut session, "SELECT * FROM orders;"), 3);
}
