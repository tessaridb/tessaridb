//! SG5 — the declaration surface (G082): `DEFINE TABLE … STORE COLUMN
//! [PARTITIONS n BY field]` defines a columnar table through the statement
//! path, INFO/BACKUP SCRIPT render the words, and the named refusals hold
//! (C1: immutability; C7: the stated limits that refuse at v1).
#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Now, Outcome, RunNow, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// A session signed into prod.analytics.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                  DEFINE DATABASE analytics; USE DATABASE analytics;",
        )
        .unwrap();
    session
}

/// The `definition` field of `INFO FOR TABLE t`.
fn definition(session: &mut Session<'_>, table: &str) -> String {
    let outcome = session
        .run_now(&format!("INFO FOR TABLE {table};"))
        .unwrap()
        .pop()
        .unwrap();
    let Outcome::Value(Value::Object(report)) = outcome else {
        panic!("expected a report");
    };
    match report.get("definition") {
        Some(Value::String(script)) => script.clone(),
        other => panic!("the report carries no definition: {other:?}"),
    }
}

#[test]
fn store_column_defines_through_the_statement_path_and_info_renders_the_words() {
    let first = store();
    let mut session = ready(&first);
    session
        .run_now("DEFINE TABLE events (kind string, seq int) STORE COLUMN PARTITIONS 4 BY kind;")
        .unwrap();
    let described = definition(&mut session, "events");
    assert!(
        described.contains("STORE COLUMN"),
        "INFO renders the word: {described}"
    );
    assert!(
        described.contains("PARTITIONS 4 BY kind"),
        "INFO renders the partitioning: {described}"
    );
    // The describe contract is restore: the definition re-executes happily
    // on an EMPTY store and the words come back the same. (Re-running it on
    // the store that holds the table is a NameTaken, as for every define.)
    let empty = store();
    let mut session = ready(&empty);
    session.run_now(&described).unwrap();
    let redescribed = definition(&mut session, "events");
    assert_eq!(described, redescribed, "the words survive a restore");
}

#[test]
fn the_default_renders_store_row_explicitly() {
    let first = store();
    let mut session = ready(&first);
    session
        .run_now("DEFINE TABLE plain (name string);")
        .unwrap();
    let described = definition(&mut session, "plain");
    assert!(
        described.contains("STORE ROW"),
        "a silence reads back as the default's word: {described}"
    );
    let empty = store();
    let mut session = ready(&empty);
    session.run_now(&described).unwrap();
    assert_eq!(described, definition(&mut session, "plain"));
}

#[test]
fn alter_table_store_refuses_with_the_named_immutability() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE TABLE events (kind string) STORE COLUMN;")
        .unwrap();
    let refused = session.run_now("ALTER TABLE events STORE ROW;");
    let Err(refused) = refused else {
        panic!("ALTER TABLE … STORE was accepted");
    };
    let text = refused.to_string();
    assert!(
        text.contains("storage kind") && text.contains("DROP and DEFINE"),
        "the refusal names the rule and the migration: {text}"
    );
}

#[test]
fn store_column_on_an_edge_table_refuses_by_name() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE TABLE base SCHEMALESS;").unwrap();
    let refused = session.run_now("DEFINE TABLE follows EDGE FROM base TO base STORE COLUMN;");
    let Err(refused) = refused else {
        panic!("STORE COLUMN on an edge table was accepted");
    };
    assert!(
        refused.to_string().contains("record-shaped"),
        "the refusal names the kind rule: {refused}"
    );
}

#[test]
fn an_undeclared_partition_key_refuses_on_a_schemafull_table() {
    let store = store();
    let mut session = ready(&store);
    let refused = session.run_now(
        "DEFINE TABLE events (kind string) SCHEMAFULL STORE COLUMN PARTITIONS 4 BY nobody;",
    );
    let Err(refused) = refused else {
        panic!("an undeclared partition key on a schemafull table was accepted");
    };
    assert!(
        refused.to_string().contains("nobody"),
        "the refusal names the field: {refused}"
    );
    // A schemaless table accepts any key: families are created on first
    // sight, so a key no record carries yet is a legitimate narrow start.
    session
        .run_now(
            "DEFINE TABLE sparse (kind string) SCHEMALESS STORE COLUMN PARTITIONS 4 BY nobody;",
        )
        .unwrap();
}

#[test]
fn a_feed_on_a_columnar_table_refuses_by_name() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE TABLE events (kind string) STORE COLUMN;")
        .unwrap();
    session
        .run_now("DEFINE TOPIC captured RETAIN 12h;")
        .unwrap();
    let refused = session.run_now("DEFINE FEED capture ON events INTO captured;");
    let Err(refused) = refused else {
        panic!("a feed on a columnar table was accepted");
    };
    assert!(
        refused.to_string().contains("COLUMN"),
        "the refusal names the storage kind: {refused}"
    );
}
