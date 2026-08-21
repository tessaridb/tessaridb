//! The catalog behaves like the rest of the store, because it *is* the rest of
//! the store.
//!
//! Every test here asserts a property that would have to be built separately if
//! catalog entries were their own keyspace: they survive a reopen, they replay
//! to a replica, they take part in a transaction, and two of them racing for one
//! name resolve the way two records racing for one key do.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use bgv_db_encoding::{decode_payload, encode_payload};
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::{Catalog, Error, RecordAddress, Store};
use bgv_db_types::{RecordId, Sequence, Value};

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

/// Create `namespace / database / table` and commit.
fn create_tree(store: &Store, names: (&str, &str, &str)) -> (u32, u32, u32) {
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace(names.0).unwrap();
    let database = catalog.create_database(namespace.id, names.1).unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, names.2)
        .unwrap();
    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    transaction.commit().unwrap();
    ids
}

#[test]
fn a_created_tree_is_readable_by_id_and_by_name() {
    let (_backend, store) = store();
    let (namespace, database, table) = create_tree(&store, ("prod", "orders", "line_items"));

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);

    let found = catalog.table(bgv_db_types::TableId::new(table)).unwrap();
    let found = found.expect("the table was committed");
    assert_eq!(found.name, "line_items");
    assert_eq!(found.namespace.get(), namespace);
    assert_eq!(found.database.get(), database);

    assert_eq!(
        catalog.namespace_id("prod").unwrap().map(|id| id.get()),
        Some(namespace)
    );
    assert_eq!(
        catalog
            .database_id(bgv_db_types::NamespaceId::new(namespace), "orders")
            .unwrap()
            .map(|id| id.get()),
        Some(database)
    );
    assert_eq!(
        catalog
            .table_id(
                bgv_db_types::NamespaceId::new(namespace),
                bgv_db_types::DatabaseId::new(database),
                "line_items"
            )
            .unwrap()
            .map(|id| id.get()),
        Some(table)
    );
}

#[test]
fn the_catalog_survives_a_reopen() {
    let (backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "line_items"));
    drop(store);

    let reopened = Store::open(backend).unwrap();
    let mut transaction = reopened.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert_eq!(
        catalog
            .table(bgv_db_types::TableId::new(table))
            .unwrap()
            .map(|found| found.name),
        Some("line_items".to_owned())
    );
}

#[test]
fn a_name_cannot_be_taken_twice_at_the_same_level() {
    let (_backend, store) = store();
    create_tree(&store, ("prod", "orders", "line_items"));

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let error = catalog.create_namespace("prod").unwrap_err();
    assert!(matches!(error, Error::NameTaken { .. }), "{error}");
}

#[test]
fn the_same_name_in_two_databases_is_two_different_tables() {
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let first = catalog.create_database(namespace.id, "a").unwrap();
    let second = catalog.create_database(namespace.id, "b").unwrap();
    let left = catalog
        .create_table(namespace.id, first.id, "users")
        .unwrap();
    let right = catalog
        .create_table(namespace.id, second.id, "users")
        .unwrap();
    assert_ne!(left.id, right.id);
    transaction.commit().unwrap();
}

#[test]
fn two_transactions_racing_for_one_name_leave_exactly_one_winner() {
    // This is the property the name record exists for. Conflict detection is
    // over writes, so without a key both transactions touch, each would write a
    // different definition and both would commit.
    let (_backend, store) = store();

    let mut first = store.begin().unwrap();
    let mut second = store.begin().unwrap();

    Catalog::new(&mut first).create_namespace("prod").unwrap();
    Catalog::new(&mut second).create_namespace("prod").unwrap();

    first.commit().unwrap();
    let error = second.commit().unwrap_err();
    assert!(matches!(error, Error::Conflict { .. }), "{error}");

    let mut check = store.begin().unwrap();
    let catalog = Catalog::new(&mut check);
    assert!(catalog.namespace_id("prod").unwrap().is_some());
}

#[test]
fn an_id_is_never_handed_out_again_after_a_drop() {
    let (_backend, store) = store();
    let (namespace, database, dropped) = create_tree(&store, ("prod", "orders", "gone"));

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    assert!(
        catalog
            .drop_table(bgv_db_types::TableId::new(dropped))
            .unwrap()
    );
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    // The name is free again...
    let recreated = catalog
        .create_table(
            bgv_db_types::NamespaceId::new(namespace),
            bgv_db_types::DatabaseId::new(database),
            "gone",
        )
        .unwrap();
    // ...but the id is not. A reused id would let a stale key resolve against a
    // different table, and nothing in the store could detect it.
    assert_ne!(recreated.id.get(), dropped);
    transaction.commit().unwrap();
}

#[test]
fn a_child_cannot_be_created_under_a_parent_that_does_not_exist() {
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);

    let error = catalog
        .create_database(bgv_db_types::NamespaceId::new(404), "orders")
        .unwrap_err();
    assert!(matches!(error, Error::NoSuchParent { .. }), "{error}");
}

#[test]
fn a_table_cannot_be_created_under_a_database_from_another_namespace() {
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let first = catalog.create_namespace("one").unwrap();
    let second = catalog.create_namespace("two").unwrap();
    let database = catalog.create_database(first.id, "orders").unwrap();

    let error = catalog
        .create_table(second.id, database.id, "users")
        .unwrap_err();
    assert!(matches!(error, Error::NoSuchParent { .. }), "{error}");
}

#[test]
fn defining_a_table_and_writing_to_it_is_one_transaction() {
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, "users")
        .unwrap();
    let address = RecordAddress::new(namespace.id, database.id, table.id, RecordId::from("u1"));
    transaction.put(
        address.clone(),
        encode_payload(&Value::from("ada")).into_bytes(),
    );
    transaction.commit().unwrap();

    let reader = store.begin().unwrap();
    let stored = reader.get(&address).unwrap().expect("the record committed");
    assert_eq!(decode_payload(&stored).unwrap(), Value::from("ada"));
}

#[test]
fn the_catalog_replays_onto_a_replica_through_the_ordinary_log() {
    // Nothing here is catalog-specific, which is the point: a definition is a
    // record, so a follower reconstructs the schema with the machinery it
    // already has.
    let (_backend, source) = store();
    let (_, _, table) = create_tree(&source, ("prod", "orders", "line_items"));

    let (_replica_backend, replica) = store();
    for (sequence, record) in source.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    let mut transaction = replica.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert_eq!(
        catalog
            .table(bgv_db_types::TableId::new(table))
            .unwrap()
            .map(|found| found.name),
        Some("line_items".to_owned())
    );
    assert!(catalog.namespace_id("prod").unwrap().is_some());
}

#[test]
fn a_reader_at_an_older_snapshot_does_not_see_a_table_defined_after_it_began() {
    // The schema is versioned because it is data. A transaction that began
    // before a table existed must not decode records against its definition.
    let (_backend, store) = store();
    let before = store.begin().unwrap();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "late"));

    let mut older = before;
    let catalog = Catalog::new(&mut older);
    assert_eq!(
        catalog.table(bgv_db_types::TableId::new(table)).unwrap(),
        None,
        "the definition was written after this snapshot"
    );
}
