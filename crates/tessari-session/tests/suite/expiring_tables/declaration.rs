//! Declaring that a table's records expire (ADR-0122 A1, A4–A6, A8).

use std::sync::Arc;

use tessari_encoding::{FormatVersion, FormatVersionKey, StoreKey, StoreValue};
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_session::{Error, Outcome};
use tessari_storage::Store;
use tessari_types::{Duration, Value};

use super::{field, info, opened, run};

#[test]
fn a_table_that_declares_expiry_reports_it_and_its_definition_recreates_it() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE TABLE message (body string) EXPIRE AFTER 7d; DEFINE COLLECTION drafts EXPIRE;",
    );
    let message = info(&mut session, "INFO FOR TABLE message;");
    let expire = field(&message, "expire");
    assert_eq!(
        field(&expire, "after"),
        Value::Duration(Duration::new(604_800, 0).unwrap())
    );
    assert_eq!(field(&expire, "state"), Value::from("on"));
    let Value::String(definition) = field(&message, "definition") else {
        panic!("no definition: {message:?}");
    };
    assert!(
        definition.contains("EXPIRE AFTER 168h"),
        "the definition re-creates the clause: {definition}"
    );
    let drafts = field(&info(&mut session, "INFO FOR TABLE drafts;"), "expire");
    assert_eq!(field(&drafts, "after"), Value::None);
    assert_eq!(field(&drafts, "state"), Value::from("on"));
}

#[test]
fn a_table_that_says_nothing_reports_no_expiry_at_all() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE plain (body string);");
    let plain = info(&mut session, "INFO FOR TABLE plain;");
    assert_eq!(field(&plain, "expire"), Value::None);
    let Value::String(definition) = field(&plain, "definition") else {
        panic!("no definition: {plain:?}");
    };
    assert!(!definition.contains("EXPIRE"), "{definition}");
}

#[test]
fn alter_turns_expiry_on_changes_the_default_and_retires_it_without_touching_records() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE TABLE session (owner string); CREATE session:1 = { owner: 'ada' };",
    );
    let Outcome::Value(answer) = run(&mut session, "ALTER TABLE session SET EXPIRE AFTER 30m;")
    else {
        panic!("ALTER … SET EXPIRE answers a value");
    };
    assert_eq!(field(&answer, "existing"), Value::from("unchanged"));
    // The record that was there before stays permanent.
    assert_eq!(
        run(&mut session, "RETURN TTL session:1;"),
        Outcome::Value(Value::Null)
    );
    run(&mut session, "ALTER TABLE session SET EXPIRE;");
    let held = field(&info(&mut session, "INFO FOR TABLE session;"), "expire");
    assert_eq!(field(&held, "after"), Value::None);
    assert_eq!(field(&held, "state"), Value::from("on"));
    let Outcome::Value(answer) = run(&mut session, "ALTER TABLE session DROP EXPIRE;") else {
        panic!("ALTER … DROP EXPIRE answers a value");
    };
    assert_eq!(
        field(&answer, "existing"),
        Value::from("still expire at their instants")
    );
    let retired = field(&info(&mut session, "INFO FOR TABLE session;"), "expire");
    assert_eq!(field(&retired, "state"), Value::from("retired"));
}

#[test]
fn expiry_is_refused_on_every_kind_but_a_table_and_a_collection() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE SPACE cache; DEFINE QUEUE jobs TIMEOUT 1m; DEFINE TABLE follows EDGE;",
    );
    for table in ["cache", "jobs", "follows"] {
        assert!(
            matches!(
                session.run(&format!("ALTER TABLE {table} SET EXPIRE AFTER 1h;")),
                Err(Error::ExpiryNotOnThisKind { .. })
            ),
            "{table}"
        );
    }
    assert!(matches!(
        session.run("DEFINE TABLE likes EDGE EXPIRE AFTER 1h;"),
        Err(Error::ExpiryNotOnThisKind { .. })
    ));
}

#[test]
fn a_store_holding_an_older_format_refuses_the_declaration_until_finalized() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    drop(Store::open(Arc::clone(&backend)).unwrap());
    let older = FormatVersion::TABLE_EXPIRY.get().checked_sub(1).unwrap();
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(older).encode(),
        ))
        .unwrap();
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = opened(&store);
    run(&mut session, "DEFINE TABLE plain (body string);");
    for script in [
        "DEFINE TABLE message (body string) EXPIRE AFTER 7d;",
        "DEFINE COLLECTION drafts EXPIRE;",
        "ALTER TABLE plain SET EXPIRE;",
    ] {
        assert!(
            matches!(session.run(script), Err(Error::FormatNotFinalized { .. })),
            "{script}"
        );
    }
    run(&mut session, "ALTER STORE FINALIZE FORMAT;");
    run(
        &mut session,
        "DEFINE TABLE message (body string) EXPIRE AFTER 7d;",
    );
}
