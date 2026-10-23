//! `SAVEPOINT`, `ROLLBACK TO` and `RELEASE SAVEPOINT` (ADR-0134 D11).

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

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE QUEUE jobs TIMEOUT 30s;",
        )
        .unwrap();
    session
}

fn count(session: &mut Session<'_>, table: &str) -> usize {
    match session
        .run_now(&format!("SELECT * FROM {table};"))
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("records, got {other:?}"),
    }
}

#[test]
fn rolling_back_to_a_savepoint_keeps_the_work_before_it() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now(
            "BEGIN; CREATE orders:1 = { n: 1 }; SAVEPOINT one;\n\
             CREATE orders:2 = { n: 2 }; UPDATE orders:1 SET n = 10; ROLLBACK TO one;\n\
             CREATE orders:3 = { n: 3 }; COMMIT;",
        )
        .unwrap();

    assert_eq!(count(&mut session, "orders"), 2);
    let found = session
        .run_now("SELECT n FROM orders:1;")
        .unwrap()
        .pop()
        .unwrap();
    assert!(format!("{found:?}").contains("Integer(1)"), "{found:?}");
}

#[test]
fn a_savepoint_can_be_rolled_back_to_again_and_later_ones_go() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now(
            "BEGIN; SAVEPOINT one; CREATE orders:1 = {}; SAVEPOINT two; CREATE orders:2 = {};\n\
             ROLLBACK TO one; CREATE orders:3 = {}; ROLLBACK TO SAVEPOINT one; COMMIT;",
        )
        .unwrap();
    assert_eq!(count(&mut session, "orders"), 0);

    let refused = session
        .run_now("BEGIN; SAVEPOINT one; SAVEPOINT two; ROLLBACK TO one; ROLLBACK TO two;")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("two"), "{refused}");
}

#[test]
fn a_released_savepoint_keeps_its_work_and_cannot_be_rolled_back_to() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now(
            "BEGIN; SAVEPOINT one; CREATE orders:1 = {}; RELEASE SAVEPOINT one; ROLLBACK TO one;",
        )
        .unwrap_err()
        .to_string();
    assert!(refused.contains("one"), "{refused}");

    session
        .run_now("BEGIN; SAVEPOINT one; CREATE orders:1 = {}; RELEASE SAVEPOINT one; COMMIT;")
        .unwrap();
    assert_eq!(count(&mut session, "orders"), 1);
}

#[test]
fn a_savepoint_belongs_to_an_open_transaction() {
    let store = store();
    let mut session = ready(&store);

    let refused = session.run_now("SAVEPOINT one;").unwrap_err().to_string();
    assert!(refused.contains("BEGIN"), "{refused}");
}

#[test]
fn a_claim_taken_since_the_savepoint_refuses_the_rollback() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("CREATE jobs:1 = { task: 'a' };").unwrap();

    let refused = session
        .run_now("BEGIN; SAVEPOINT one; CLAIM FROM jobs; ROLLBACK TO one;")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("claim"), "{refused}");
}
