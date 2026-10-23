//! `DEFINE FIELD … TYPE record REFERENCES t [ON DELETE …]` (ADR-0134 D10).

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

/// Two users and an order for the first, the order's owner declared with `rule`.
fn ready<'a>(store: &'a Store, rule: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run_now(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users; DEFINE COLLECTION orders; DEFINE COLLECTION log;\n\
             DEFINE FIELD owner ON orders TYPE record REFERENCES users {rule};\n\
             CREATE users:1 = {{ name: 'ada' }}; CREATE users:2 = {{ name: 'grace' }};\n\
             CREATE orders:1 = {{ owner: users:1, total: 10 }};"
        ))
        .unwrap();
    session
}

fn count(session: &mut Session<'_>, statement: &str) -> usize {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("records, got {other:?}"),
    }
}

fn refused(session: &mut Session<'_>, statement: &str) -> String {
    session.run_now(statement).unwrap_err().to_string()
}

#[test]
fn a_reference_must_name_a_record_the_table_holds() {
    let store = store();
    let mut session = ready(&store, "");

    session
        .run_now("CREATE orders:2 = { owner: users:2 }; CREATE orders:3 = { total: 1 };")
        .unwrap();
    let missing = refused(&mut session, "CREATE orders:4 = { owner: users:9 };");
    assert!(missing.contains("users:9"), "{missing}");
    let elsewhere = refused(&mut session, "CREATE orders:5 = { owner: log:1 };");
    assert!(elsewhere.contains("users"), "{elsewhere}");
    assert_eq!(count(&mut session, "SELECT * FROM orders;"), 3);
}

#[test]
fn a_target_written_in_the_same_transaction_is_found() {
    let store = store();
    let mut session = ready(&store, "");

    session
        .run_now("BEGIN; CREATE users:9 = {}; CREATE orders:9 = { owner: users:9 }; COMMIT;")
        .unwrap();
    assert_eq!(count(&mut session, "SELECT * FROM orders;"), 2);
}

#[test]
fn reject_refuses_the_delete_while_a_record_names_it() {
    let store = store();
    let mut session = ready(&store, "ON DELETE REJECT");

    let still = refused(&mut session, "DELETE users:1;");
    assert!(still.contains("orders"), "{still}");
    let by_set = refused(&mut session, "DELETE FROM users WHERE name = 'ada';");
    assert!(by_set.contains("orders"), "{by_set}");

    session
        .run_now("DELETE users:2; DELETE orders:1; DELETE users:1;")
        .unwrap();
    assert_eq!(count(&mut session, "SELECT * FROM users;"), 0);
}

#[test]
fn reject_is_what_an_unwritten_rule_means() {
    let store = store();
    let mut session = ready(&store, "");

    let still = refused(&mut session, "DELETE users:1;");
    assert!(still.contains("orders"), "{still}");
}

#[test]
fn cascade_deletes_the_records_naming_it_and_runs_their_events() {
    let store = store();
    let mut session = ready(&store, "ON DELETE CASCADE");
    session
        .run_now(
            "DEFINE EVENT gone ON orders FOR DELETE THEN CREATE log = { order: $id };\n\
             CREATE orders:2 = { owner: users:1 }; CREATE orders:3 = { owner: users:2 };",
        )
        .unwrap();

    session.run_now("DELETE users:1;").unwrap();
    assert_eq!(count(&mut session, "SELECT * FROM orders;"), 1);
    assert_eq!(count(&mut session, "SELECT * FROM log;"), 2);
}

#[test]
fn unset_clears_the_field_and_keeps_the_record() {
    let store = store();
    let mut session = ready(&store, "ON DELETE UNSET");

    session
        .run_now("DELETE FROM users WHERE name = 'ada';")
        .unwrap();
    assert_eq!(
        count(&mut session, "SELECT * FROM orders WHERE owner = NONE;"),
        1
    );
    assert_eq!(
        count(&mut session, "SELECT * FROM orders WHERE total = 10;"),
        1
    );
}

#[test]
fn a_referenced_table_is_neither_truncated_nor_dropped() {
    let store = store();
    let mut session = ready(&store, "ON DELETE CASCADE");

    let truncated = refused(&mut session, "TRUNCATE users;");
    assert!(truncated.contains("orders.owner"), "{truncated}");
    let dropped = refused(&mut session, "DROP TABLE users;");
    assert!(dropped.contains("orders.owner"), "{dropped}");

    session
        .run_now("DROP FIELD owner ON orders; TRUNCATE users;")
        .unwrap();
}

#[test]
fn a_rename_keeps_the_reference() {
    let store = store();
    let mut session = ready(&store, "");

    session
        .run_now("ALTER TABLE users RENAME TO people;")
        .unwrap();
    session
        .run_now("CREATE orders:2 = { owner: people:2 };")
        .unwrap();
    let missing = refused(&mut session, "CREATE orders:3 = { owner: people:9 };");
    assert!(missing.contains("people:9"), "{missing}");
}

#[test]
fn a_reference_is_declared_over_a_record_field_into_a_table_that_exists() {
    let store = store();
    let mut session = ready(&store, "");

    let nowhere = refused(
        &mut session,
        "DEFINE FIELD buyer ON orders TYPE record REFERENCES nobody;",
    );
    assert!(nowhere.contains("nobody"), "{nowhere}");
    let typed = refused(
        &mut session,
        "DEFINE FIELD label ON orders TYPE string REFERENCES users;",
    );
    assert!(typed.contains("record"), "{typed}");
}
