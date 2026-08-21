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
use bgv_db_storage::{Catalog, Error, RecordAddress, Store, TableShape};
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
        .create_table(namespace.id, database.id, names.2, TableShape::default())
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
        .create_table(namespace.id, first.id, "users", TableShape::default())
        .unwrap();
    let right = catalog
        .create_table(namespace.id, second.id, "users", TableShape::default())
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
            TableShape::default(),
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
        .create_table(second.id, database.id, "users", TableShape::default())
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
        .create_table(namespace.id, database.id, "users", TableShape::default())
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

#[test]
fn an_index_is_created_on_a_table_and_found_by_it() {
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "users"));
    let table = bgv_db_types::TableId::new(table);

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let by_email = catalog
        .create_index(table, "by_email", vec!["email".to_owned()], true)
        .unwrap();
    let by_name = catalog
        .create_index(
            table,
            "by_name",
            vec!["last".to_owned(), "first".to_owned()],
            false,
        )
        .unwrap();
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert_eq!(catalog.index(by_email.id).unwrap(), Some(by_email.clone()));

    let mut found = catalog.indexes_on(table).unwrap();
    found.sort_by(|left, right| left.name.cmp(&right.name));
    assert_eq!(found, vec![by_email, by_name]);
    assert!(
        catalog
            .indexes_on(bgv_db_types::TableId::new(999))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_index_over_no_fields_is_refused() {
    // One entry for the whole table is not a degenerate index; a unique one
    // would admit a single record and refuse every other.
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "users"));

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let error = catalog
        .create_index(bgv_db_types::TableId::new(table), "empty", vec![], false)
        .unwrap_err();
    assert!(matches!(error, Error::EmptyIndex { .. }), "{error}");
}

#[test]
fn two_indexes_on_one_table_cannot_share_a_name_but_two_tables_can() {
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let users = catalog
        .create_table(namespace.id, database.id, "users", TableShape::default())
        .unwrap();
    let carts = catalog
        .create_table(namespace.id, database.id, "carts", TableShape::default())
        .unwrap();

    catalog
        .create_index(users.id, "by_id", vec!["id".to_owned()], true)
        .unwrap();
    let error = catalog
        .create_index(users.id, "by_id", vec!["other".to_owned()], false)
        .unwrap_err();
    assert!(matches!(error, Error::NameTaken { .. }), "{error}");

    // The same name on another table is a different index.
    catalog
        .create_index(carts.id, "by_id", vec!["id".to_owned()], true)
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn a_scan_returns_live_records_including_this_transactions_own_writes() {
    let (_backend, store) = store();
    let (namespace, database, table) = create_tree(&store, ("prod", "orders", "users"));
    let namespace = bgv_db_types::NamespaceId::new(namespace);
    let database = bgv_db_types::DatabaseId::new(database);
    let table = bgv_db_types::TableId::new(table);
    let at = |id: &str| RecordAddress::new(namespace, database, table, RecordId::from(id));

    let mut transaction = store.begin().unwrap();
    transaction.put(at("a"), encode_payload(&Value::from(1_i64)).into_bytes());
    transaction.put(at("b"), encode_payload(&Value::from(2_i64)).into_bytes());
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    // A newer version replaces the older one, a delete removes the record from
    // the answer, and an uncommitted write is visible to its own transaction.
    transaction.put(at("a"), encode_payload(&Value::from(11_i64)).into_bytes());
    transaction.delete(at("b"));
    transaction.put(at("c"), encode_payload(&Value::from(3_i64)).into_bytes());

    let live = transaction.scan_table(namespace, database, table).unwrap();
    let seen: Vec<(String, Value)> = live
        .into_iter()
        .map(|(id, payload)| (id.to_string(), decode_payload(&payload).unwrap()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("a".to_owned(), Value::from(11_i64)),
            ("c".to_owned(), Value::from(3_i64)),
        ]
    );
}

#[test]
fn a_scan_at_an_older_snapshot_does_not_see_later_writes() {
    let (_backend, store) = store();
    let (namespace, database, table) = create_tree(&store, ("prod", "orders", "users"));
    let namespace = bgv_db_types::NamespaceId::new(namespace);
    let database = bgv_db_types::DatabaseId::new(database);
    let table = bgv_db_types::TableId::new(table);
    let at = |id: &str| RecordAddress::new(namespace, database, table, RecordId::from(id));

    let mut first = store.begin().unwrap();
    first.put(at("a"), encode_payload(&Value::from(1_i64)).into_bytes());
    first.commit().unwrap();

    let reader = store.begin().unwrap();

    let mut later = store.begin().unwrap();
    later.put(at("a"), encode_payload(&Value::from(2_i64)).into_bytes());
    later.put(at("b"), encode_payload(&Value::from(9_i64)).into_bytes());
    later.commit().unwrap();

    let live = reader.scan_table(namespace, database, table).unwrap();
    assert_eq!(live.len(), 1, "the reader began before the second commit");
    assert_eq!(
        decode_payload(&live[0].1).unwrap(),
        Value::from(1_i64),
        "and sees the version that was current then"
    );
}

#[test]
fn an_edge_table_carries_an_index_on_each_endpoint_from_the_moment_it_exists() {
    // Traversal is an index read, so an edge table whose indexes the caller had
    // to remember to declare would traverse for some callers and scan for
    // others. The declaration creates them, in the same commit.
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let follows = catalog
        .create_table(
            namespace.id,
            database.id,
            "follows",
            TableShape {
                edge: true,
                ..TableShape::default()
            },
        )
        .unwrap();
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let mut indexed: Vec<Vec<String>> = catalog
        .indexes_on(follows.id)
        .unwrap()
        .into_iter()
        .map(|index| index.fields)
        .collect();
    indexed.sort();
    assert_eq!(
        indexed,
        vec![vec!["in".to_owned()], vec!["out".to_owned()]],
        "an edge table needs both directions"
    );
    assert!(catalog.table(follows.id).unwrap().unwrap().edge);

    // And each endpoint is declared, so an edge table can also be schemafull
    // without the caller declaring fields the store itself fills in.
    let mut declared: Vec<(String, bgv_db_types::FieldKind)> = catalog
        .fields_on(follows.id)
        .unwrap()
        .into_iter()
        .map(|field| (field.name, field.kind))
        .collect();
    declared.sort();
    assert_eq!(
        declared,
        vec![
            ("in".to_owned(), bgv_db_types::FieldKind::Record),
            ("out".to_owned(), bgv_db_types::FieldKind::Record),
        ]
    );
}

#[test]
fn a_plain_table_gets_no_indexes_it_did_not_ask_for() {
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "users"));
    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert!(
        catalog
            .indexes_on(bgv_db_types::TableId::new(table))
            .unwrap()
            .is_empty()
    );
    assert!(
        catalog
            .fields_on(bgv_db_types::TableId::new(table))
            .unwrap()
            .is_empty()
    );
    assert!(
        !catalog
            .table(bgv_db_types::TableId::new(table))
            .unwrap()
            .unwrap()
            .edge
    );
}

#[test]
fn a_replica_rebuilds_an_edge_table_with_its_indexes_and_its_declarations() {
    // Declaring an edge table writes five catalog records in one commit — the
    // table, two indexes and two field declarations. A replica has only that log
    // record, so this is where the shape could be built on the leader and not on
    // the replica.
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "social").unwrap();
    let users = catalog
        .create_table(namespace.id, database.id, "users", TableShape::default())
        .unwrap();
    let follows = catalog
        .create_table(
            namespace.id,
            database.id,
            "follows",
            TableShape {
                edge: true,
                ..TableShape::default()
            },
        )
        .unwrap();
    transaction.commit().unwrap();

    // An edge, so the endpoint indexes have an entry to disagree about.
    let mut transaction = store.begin().unwrap();
    let edge = Value::Object(
        [
            (
                "out".to_owned(),
                Value::Record(bgv_db_types::RecordRef::new(users.id, RecordId::Int(1))),
            ),
            (
                "in".to_owned(),
                Value::Record(bgv_db_types::RecordRef::new(users.id, RecordId::Int(2))),
            ),
        ]
        .into_iter()
        .collect(),
    );
    transaction.put(
        RecordAddress::new(namespace.id, database.id, follows.id, RecordId::Int(1)),
        encode_payload(&edge).into_bytes(),
    );
    transaction.commit().unwrap();

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    for (sequence, record) in store.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    let mut transaction = replica.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert!(catalog.table(follows.id).unwrap().unwrap().edge);
    assert_eq!(catalog.indexes_on(follows.id).unwrap().len(), 2);
    assert_eq!(catalog.fields_on(follows.id).unwrap().len(), 2);

    // And the entry the index holds is the same one, so a traversal on the
    // replica answers what it answers on the leader.
    let index = catalog
        .indexes_on(follows.id)
        .unwrap()
        .into_iter()
        .find(|index| index.fields == vec!["out".to_owned()])
        .expect("an edge table has an index on out");
    let anchor = Value::Record(bgv_db_types::RecordRef::new(users.id, RecordId::Int(1)));
    assert_eq!(
        transaction
            .records_by_index(&index, &[anchor])
            .unwrap()
            .len(),
        1
    );
}
