//! The retention floor, and the leak that would pin it forever.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use bgv_db_encoding::encode_payload;
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::{Catalog, RecordAddress, Store};
use bgv_db_types::{RecordId, Sequence, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A namespace, database and table, committed.
fn tree(store: &Store) -> (u32, u32, u32) {
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, "users", false)
        .unwrap();
    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    transaction.commit().unwrap();
    ids
}

/// Write one version of a record and return the sequence it committed at.
fn write(store: &Store, at: &RecordAddress, name: &str) -> Sequence {
    let mut transaction = store.begin().unwrap();
    let mut object = std::collections::BTreeMap::new();
    object.insert("name".to_owned(), Value::String(name.to_owned()));
    transaction.put(
        at.clone(),
        encode_payload(&Value::Object(object)).into_bytes(),
    );
    transaction.commit().unwrap()
}

#[test]
fn the_floor_is_the_committed_tail_when_nothing_is_being_read() {
    let store = store();
    let _ = tree(&store);
    assert_eq!(store.live_snapshots(), 0);
    assert_eq!(
        store.retention_floor().unwrap(),
        store.committed_tail().unwrap()
    );
    assert_eq!(store.oldest_snapshot_age(), None);
}

#[test]
fn a_live_reader_holds_the_floor_where_it_began() {
    let store = store();
    let (namespace, database, table) = tree(&store);
    let at = RecordAddress::new(
        bgv_db_types::NamespaceId::new(namespace),
        bgv_db_types::DatabaseId::new(database),
        bgv_db_types::TableId::new(table),
        RecordId::Int(1),
    );

    write(&store, &at, "ada");
    let reader = store.begin().unwrap();
    let held = reader.snapshot();

    // The store moves on while the reader is still working.
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    assert!(store.committed_tail().unwrap() > held);
    assert_eq!(
        store.retention_floor().unwrap(),
        held,
        "the floor may not pass a reader that is still reading"
    );
    assert_eq!(store.live_snapshots(), 1);
    assert!(store.oldest_snapshot_age().is_some());

    drop(reader);
    assert_eq!(
        store.retention_floor().unwrap(),
        store.committed_tail().unwrap()
    );
}

#[test]
fn a_transaction_dropped_without_commit_or_rollback_releases_its_snapshot() {
    // This is the leak the design is shaped around. A transaction that is simply
    // let go of would otherwise pin the floor for the life of the process — not
    // with an error, but as space that never comes back.
    let store = store();
    let _ = tree(&store);
    let tail = store.committed_tail().unwrap();

    {
        let transaction = store.begin().unwrap();
        assert_eq!(store.live_snapshots(), 1);
        // No commit. No rollback. Just let go.
        let _ = transaction.snapshot();
    }

    assert_eq!(
        store.live_snapshots(),
        0,
        "the snapshot outlived its reader"
    );
    assert_eq!(store.retention_floor().unwrap(), tail);
}

#[test]
fn a_committed_transaction_releases_its_snapshot_too() {
    let store = store();
    let (namespace, database, table) = tree(&store);
    let at = RecordAddress::new(
        bgv_db_types::NamespaceId::new(namespace),
        bgv_db_types::DatabaseId::new(database),
        bgv_db_types::TableId::new(table),
        RecordId::Int(1),
    );

    write(&store, &at, "ada");
    assert_eq!(store.live_snapshots(), 0);

    let transaction = store.begin().unwrap();
    transaction.rollback();
    assert_eq!(store.live_snapshots(), 0);
}

#[test]
fn two_readers_at_one_sequence_both_have_to_finish_before_the_floor_moves() {
    let store = store();
    let _ = tree(&store);
    let first = store.begin().unwrap();
    let second = store.begin().unwrap();
    let held = first.snapshot();
    assert_eq!(second.snapshot(), held, "both began at the same tail");
    assert_eq!(store.live_snapshots(), 1, "one sequence, two holders");

    drop(first);
    assert_eq!(
        store.retention_floor().unwrap(),
        held,
        "one reader is still working at it"
    );

    drop(second);
    assert_eq!(store.live_snapshots(), 0);
}

#[test]
fn a_cloned_store_shares_one_registry_because_it_is_one_store() {
    // A floor computed from half the live readers would reclaim versions the
    // other half is still reading.
    let store = store();
    let _ = tree(&store);
    let clone = store.clone();

    let reader = clone.begin().unwrap();
    let held = reader.snapshot();
    assert_eq!(store.live_snapshots(), 1);
    assert_eq!(store.retention_floor().unwrap(), held);

    drop(reader);
    assert_eq!(store.live_snapshots(), 0);
}

fn record(store: &Store, namespace: u32, database: u32, table: u32, id: i64) -> RecordAddress {
    let _ = store;
    RecordAddress::new(
        bgv_db_types::NamespaceId::new(namespace),
        bgv_db_types::DatabaseId::new(database),
        bgv_db_types::TableId::new(table),
        RecordId::Int(id),
    )
}

/// The `name` field of the record a reader resolves to.
fn name_seen_by(reader: &bgv_db_storage::Transaction<'_>, at: &RecordAddress) -> Option<String> {
    let payload = reader.get(at).unwrap()?;
    let Value::Object(object) = bgv_db_encoding::decode_payload(&payload).unwrap() else {
        panic!("expected an object");
    };
    match object.get("name") {
        Some(Value::String(text)) => Some(text.clone()),
        other => panic!("unexpected name: {other:?}"),
    }
}

#[test]
fn reclaiming_removes_older_versions_and_keeps_what_a_reader_at_the_floor_sees() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.versions, 2, "two versions nobody can reach");
    assert_eq!(removed.records, 0);

    // Asserted by reading rather than by counting keys, because over-reclaiming
    // does not raise anything — it just answers with an older value.
    let reader = store.begin().unwrap();
    assert_eq!(name_seen_by(&reader, &at).as_deref(), Some("hopper"));
}

#[test]
fn a_held_snapshot_keeps_every_version_it_can_still_reach() {
    // The registry's whole purpose: reclaiming while a reader is mid-work must
    // not change what that reader sees.
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    let reader = store.begin().unwrap();
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(
        removed.versions, 0,
        "the reader holds the floor, and nothing is older than what it sees"
    );
    assert_eq!(name_seen_by(&reader, &at).as_deref(), Some("ada"));

    // Once it finishes, the same pass can do its work.
    drop(reader);
    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.versions, 2);

    let after = store.begin().unwrap();
    assert_eq!(name_seen_by(&after, &at).as_deref(), Some("hopper"));
}

#[test]
fn a_deleted_record_stops_costing_space() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    write(&store, &at, "grace");
    let mut transaction = store.begin().unwrap();
    transaction.delete(at.clone());
    transaction.commit().unwrap();

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.records, 1, "the tombstone itself went too");
    assert_eq!(removed.versions, 3, "two writes and the tombstone");

    // A reader that finds a tombstone and one that finds nothing reach the same
    // conclusion, which is what makes removing it safe.
    let reader = store.begin().unwrap();
    assert!(reader.get(&at).unwrap().is_none());
}

#[test]
fn reclaiming_twice_removes_nothing_the_second_time() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);
    write(&store, &at, "ada");
    write(&store, &at, "grace");

    let first = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert!(first.versions > 0);
    let second = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(second.versions, 0);
}

#[test]
fn reclaiming_does_not_disturb_a_neighbouring_record() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let first = record(&store, ns, db, tb, 1);
    let second = record(&store, ns, db, tb, 2);

    write(&store, &first, "ada");
    write(&store, &first, "grace");
    write(&store, &second, "hopper");

    let removed = store
        .reclaim_table(first.namespace, first.database, first.table)
        .unwrap();
    assert_eq!(removed.versions, 1, "only the superseded one");

    let reader = store.begin().unwrap();
    assert_eq!(name_seen_by(&reader, &first).as_deref(), Some("grace"));
    assert_eq!(name_seen_by(&reader, &second).as_deref(), Some("hopper"));
}
