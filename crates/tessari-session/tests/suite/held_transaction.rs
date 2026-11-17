//! A transaction held across bridge calls.
//!
//! The wire port answers `BEGIN` with a transaction that must stay open while
//! the connection runs other statements: the session parks the open
//! transaction in its detached state, and the next call's statements join its
//! snapshot instead of opening new ones. These tests sit at that birth site —
//! the session API a surface holds across its bridge — not at a socket.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Now;
use tessari_session::{Error, RunNow, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// A session with `prod / orders` defined and selected.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;",
        )
        .unwrap();
    session
}

/// A session that only selects what [`ready`] defined — the shape a second
/// connection takes against one store.
fn selected(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
}

#[test]
fn a_begin_parks_the_transaction_across_detach() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE TABLE users SCHEMALESS;").unwrap();

    session.run_holding_now("BEGIN").unwrap();
    assert!(session.transaction_open());

    // Across the bridge call the transaction rides the detached state.
    let detached = session.detach();
    assert!(detached.transaction_open());
    let mut session = detached.attach(&store);
    assert!(session.transaction_open());

    // Its own writes are visible to its own reads, in the later call.
    session
        .run_holding_now("CREATE users:1 = { name: 'ada' };")
        .unwrap();
    let outcomes = session.run_holding_now("SELECT * FROM users:1;").unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 1);
    assert!(session.transaction_open());

    // COMMIT in a third call lands what the first buffered.
    let detached = session.detach();
    let mut session = detached.attach(&store);
    session.run_holding_now("COMMIT;").unwrap();
    assert!(!session.transaction_open());

    // A fresh session reads what the held block committed.
    let mut fresh = selected(&store);
    let outcomes = fresh.run_now("SELECT * FROM users:1;").unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 1);
}

#[test]
fn a_held_transaction_rolls_back_with_cancel() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE TABLE users SCHEMALESS;").unwrap();

    session.run_holding_now("BEGIN;").unwrap();
    let detached = session.detach();
    let mut session = detached.attach(&store);
    session
        .run_holding_now("CREATE users:1 = { name: 'ada' };")
        .unwrap();
    session.run_holding_now("CANCEL;").unwrap();
    assert!(!session.transaction_open());

    let mut fresh = selected(&store);
    let outcomes = fresh.run_now("SELECT * FROM users:1;").unwrap();
    assert!(outcomes[0].records().unwrap().is_empty());
}

#[test]
fn a_held_transaction_reads_one_snapshot_across_calls() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE TABLE users SCHEMALESS; CREATE users:1 = { name: 'ada' };")
        .unwrap();

    // The held block begins, then another session commits over the record.
    session.run_holding_now("BEGIN;").unwrap();
    let mut other = selected(&store);
    other.run_now("UPDATE users:1 SET name = 'grace';").unwrap();

    // The held session still reads the point its snapshot took.
    let outcomes = session.run_holding_now("SELECT * FROM users:1;").unwrap();
    let records = outcomes[0].records().unwrap();
    let Value::Object(object) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(object.get("name"), Some(&Value::String("ada".to_owned())));

    // Discarding the block discards its buffered view with it.
    session.run_holding_now("CANCEL;").unwrap();
}

#[test]
fn a_second_begin_while_held_refuses_nested() {
    let store = store();
    let mut session = ready(&store);
    session.run_holding_now("BEGIN;").unwrap();
    let error = session.run_holding_now("BEGIN;").unwrap_err();
    assert!(
        matches!(error, Error::NestedTransaction { .. }),
        "unexpected: {error:?}"
    );
}

#[test]
fn commit_with_nothing_held_refuses() {
    let store = store();
    let mut session = ready(&store);
    let error = session.run_holding_now("COMMIT;").unwrap_err();
    assert!(
        matches!(error, Error::NoOpenTransaction { .. }),
        "unexpected: {error:?}"
    );
}

#[test]
fn a_refusal_inside_the_block_drops_the_held_transaction() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE TABLE users SCHEMAFULL;").unwrap();

    session.run_holding_now("BEGIN;").unwrap();
    // The write buffers; a field the schemafull table does not declare
    // refuses at the commit that maintains the schema — inside the block.
    session
        .run_holding_now("CREATE users:1 = { unknown: 1 };")
        .unwrap();
    let error = session.run_holding_now("COMMIT;").unwrap_err();
    assert!(
        matches!(error, Error::Store(_) | Error::UndeclaredField { .. }),
        "unexpected: {error:?}"
    );
    // The abort the dialect counts: nothing remains held.
    assert!(!session.transaction_open());
    let mut fresh = selected(&store);
    let outcomes = fresh.run_now("SELECT * FROM users;").unwrap();
    assert!(outcomes[0].records().unwrap().is_empty());
}

#[test]
fn native_run_still_refuses_an_unclosed_begin() {
    let store = store();
    let mut session = ready(&store);
    // The published promise is untouched: a script that ends open refuses.
    let error = session.run_now("BEGIN;").unwrap_err();
    assert!(
        matches!(error, Error::UnclosedTransaction { .. }),
        "unexpected: {error:?}"
    );
}
