//! Generated identities that spread over a table's shards — G053 SG4a,
//! ADR-0113 D1.
//!
//! A UUID v7 is time-ordered and sorts apart from text split points, so every
//! record a table names itself lands in one shard. `IDENTITY uuid SPREAD`
//! begins the identity with a bucket of two hex digits, and the shards share
//! the buckets.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::{Catalog, Store};
use tessari_types::{RecordId, Value};

const RECORDS: usize = 400;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn tenancy(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;")
        .unwrap();
    session
}

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    match session.run(read).unwrap().last() {
        Some(Outcome::Records { records, .. }) => {
            records.iter().map(|(id, _)| id.clone()).collect()
        }
        other => panic!("{read}: {other:?}"),
    }
}

/// How many of `orders`' records each of its shards holds, by the table's own
/// shard map.
fn per_shard(store: &Store, session: &mut Session<'_>) -> BTreeMap<u32, usize> {
    let named = ids(session, "SELECT * FROM orders;");
    assert_eq!(named.len(), RECORDS);
    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "orders")
        .unwrap()
        .unwrap();
    let map = catalog.table(table).unwrap().unwrap().shards.unwrap();
    let mut counted = BTreeMap::new();
    for id in &named {
        let held = counted.entry(map.shard_of(id).get()).or_insert(0_usize);
        *held = held.saturating_add(1);
    }
    transaction.rollback();
    counted
}

fn written(session: &mut Session<'_>, declaration: &str) {
    session.run(declaration).unwrap();
    let creates = "CREATE orders = { n: 1 };".repeat(RECORDS);
    session.run(&creates).unwrap();
}

#[test]
fn a_spread_table_lands_its_new_records_in_every_shard() {
    let store = store();
    let mut session = tenancy(&store);
    written(
        &mut session,
        "DEFINE TABLE orders (n int) IDENTITY uuid SPREAD SPLIT AT '40', '80', 'c0';",
    );
    let counted = per_shard(&store, &mut session);
    // Even is a hundred each; sixty is more than four standard deviations
    // below it, so a fair bucket never fails this and a skewed one does.
    assert_eq!(counted.len(), 4, "{counted:?}");
    assert!(counted.values().all(|held| *held >= 60), "{counted:?}");
    // Each identity is a bucket and the UUID after it.
    for id in ids(&mut session, "SELECT * FROM orders LIMIT 5;") {
        let RecordId::Text(text) = &id else {
            panic!("a spread identity is text: {id:?}");
        };
        let (bucket, uuid) = text.split_once(':').unwrap();
        assert!(bucket.len() == 2 && bucket.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(uuid.len(), 36, "{text}");
    }
}

#[test]
fn without_spread_every_new_record_lands_in_one_shard() {
    // The hot spot the option exists for, measured the same way.
    let store = store();
    let mut session = tenancy(&store);
    written(
        &mut session,
        "DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT '40', '80', 'c0';",
    );
    let counted = per_shard(&store, &mut session);
    assert_eq!(
        counted.values().copied().max(),
        Some(RECORDS),
        "{counted:?}"
    );
}

#[test]
fn a_region_spreads_within_itself() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE customers (region string) IDENTITY uuid SPREAD PARTITION BY region; \
             CREATE customers = { region: 'de' };",
        )
        .unwrap();
    let named = ids(&mut session, "SELECT * FROM customers WHERE region = 'de';");
    let [RecordId::Text(text)] = named.as_slice() else {
        panic!("{named:?}");
    };
    let parts: Vec<&str> = text.splitn(3, ':').collect();
    assert_eq!(parts.first(), Some(&"de"), "{text}");
    assert_eq!(parts.get(1).map(|bucket| bucket.len()), Some(2), "{text}");
}

#[test]
fn spread_needs_the_store_to_generate_a_uuid() {
    let store = store();
    let mut session = tenancy(&store);
    match session.run("DEFINE TABLE orders (n int) IDENTITY int SPREAD;") {
        Err(Error::Store(tessari_storage::Error::SpreadNeedsGeneratedUuid { table })) => {
            assert_eq!(table.as_str(), "orders");
        }
        other => panic!("expected SpreadNeedsGeneratedUuid, got {other:?}"),
    }
}

#[test]
fn the_report_says_spread_and_its_definition_re_creates_it() {
    let original = store();
    let mut session = tenancy(&original);
    session
        .run("DEFINE TABLE orders (n int) IDENTITY uuid SPREAD;")
        .unwrap();
    let described = match session.run("INFO FOR TABLE orders;").unwrap().last() {
        Some(Outcome::Value(value)) => value.clone(),
        other => panic!("{other:?}"),
    };
    let Value::Object(fields) = &described else {
        panic!("{described:?}");
    };
    assert_eq!(fields.get("spread"), Some(&Value::Bool(true)));
    let Some(Value::String(script)) = fields.get("definition") else {
        panic!("{described:?}");
    };
    let copy = store();
    let mut again = tenancy(&copy);
    again.run(script).unwrap();
    again.run("CREATE orders = { n: 1 };").unwrap();
    let named = ids(&mut again, "SELECT * FROM orders;");
    assert!(
        matches!(named.as_slice(), [RecordId::Text(text)] if text.as_bytes().get(2) == Some(&b':')),
        "the restored table does not spread: {script} → {named:?}"
    );
}
