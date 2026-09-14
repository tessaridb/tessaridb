//! The catalog behaves like the rest of the store, because it *is* the rest of
//! the store.
//!
//! Every test here asserts a property that would have to be built separately if
//! catalog entries were their own keyspace: they survive a reopen, they replay
//! to a replica, they take part in a transaction, and two of them racing for one
//! name resolve the way two records racing for one key do.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, Error, IndexShape, RecordAddress, Store, TableKind, TableShape};
use tessari_types::{Path, RecordId, Value};

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

    let found = catalog.table(tessari_types::TableId::new(table)).unwrap();
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
            .database_id(tessari_types::NamespaceId::new(namespace), "orders")
            .unwrap()
            .map(|id| id.get()),
        Some(database)
    );
    assert_eq!(
        catalog
            .table_id(
                tessari_types::NamespaceId::new(namespace),
                tessari_types::DatabaseId::new(database),
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
            .table(tessari_types::TableId::new(table))
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
fn the_same_name_in_two_namespaces_is_two_different_databases() {
    // The property a tenant expects and nothing else asserts: `docs` in one
    // namespace and `docs` in another are separate databases, so a second tenant
    // is not refused a name because a first one took it. The store-wide levels
    // are namespaces, users, analyzers, consumers and replicas — a database is
    // not one of them, and this pins that rather than leaving it to the reading
    // of `qualify`.
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let first = catalog.create_namespace("alpha").unwrap();
    let second = catalog.create_namespace("beta").unwrap();
    let left = catalog.create_database(first.id, "docs").unwrap();
    let right = catalog.create_database(second.id, "docs").unwrap();
    assert_ne!(left.id, right.id);
    // And each name still resolves inside its own namespace, which is the half
    // that would fail if the two entries collided on one key.
    assert_eq!(
        catalog.database_id(first.id, "docs").unwrap(),
        Some(left.id)
    );
    assert_eq!(
        catalog.database_id(second.id, "docs").unwrap(),
        Some(right.id)
    );
    transaction.commit().unwrap();
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
            .drop_table(tessari_types::TableId::new(dropped))
            .unwrap()
    );
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    // The name is free again...
    let recreated = catalog
        .create_table(
            tessari_types::NamespaceId::new(namespace),
            tessari_types::DatabaseId::new(database),
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
        .create_database(tessari_types::NamespaceId::new(404), "orders")
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
    crate::replay(&source, &replica);

    let mut transaction = replica.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert_eq!(
        catalog
            .table(tessari_types::TableId::new(table))
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
        catalog.table(tessari_types::TableId::new(table)).unwrap(),
        None,
        "the definition was written after this snapshot"
    );
}

#[test]
fn a_reader_at_an_older_snapshot_sees_a_table_altered_after_it_began_as_it_was() {
    // The other half of the property above, and the half that a schema cache
    // would be free to break: a table that EXISTED when the reader began and
    // was altered afterwards. Absence is conspicuous — a `None` where a table
    // was expected fails loudly. A definition that is merely the WRONG VERSION
    // decodes records against a shape nobody asked for and reports nothing.
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "invoices"));
    let table = tessari_types::TableId::new(table);

    let mut older = store.begin().unwrap();
    // Read once before the alteration, so this reader has already answered for
    // the definition it is entitled to.
    let as_it_began = Catalog::new(&mut older)
        .table(table)
        .unwrap()
        .expect("the table was committed before this snapshot")
        .schemafull;

    let mut altering = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut altering);
    assert!(
        catalog.set_schemafull(table, !as_it_began).unwrap(),
        "the alteration must have taken effect, or this test proves nothing"
    );
    altering.commit().unwrap();

    // A second transaction sees the new definition, which is what makes the
    // assertion below a statement about snapshots rather than about caching.
    let mut newer = store.begin().unwrap();
    assert_eq!(
        Catalog::new(&mut newer)
            .table(table)
            .unwrap()
            .expect("the table still exists")
            .schemafull,
        !as_it_began,
        "a reader that began after the alteration sees it"
    );

    assert_eq!(
        Catalog::new(&mut older)
            .table(table)
            .unwrap()
            .expect("the table still exists at the older snapshot")
            .schemafull,
        as_it_began,
        "the older reader must still see the definition as of its own snapshot"
    );
}

#[test]
fn an_index_is_created_on_a_table_and_found_by_it() {
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "users"));
    let table = tessari_types::TableId::new(table);

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let by_email = catalog
        .create_index(
            table,
            "by_email",
            vec![Path::field("email")],
            IndexShape {
                unique: true,
                search: false,
                spatial: false,
                vector: None,
            },
        )
        .unwrap();
    let by_name = catalog
        .create_index(
            table,
            "by_name",
            vec![Path::field("last"), Path::field("first")],
            IndexShape::default(),
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
            .indexes_on(tessari_types::TableId::new(999))
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
        .create_index(
            tessari_types::TableId::new(table),
            "empty",
            vec![],
            IndexShape::default(),
        )
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
        .create_index(
            users.id,
            "by_id",
            vec![Path::field("id")],
            IndexShape {
                unique: true,
                search: false,
                spatial: false,
                vector: None,
            },
        )
        .unwrap();
    let error = catalog
        .create_index(
            users.id,
            "by_id",
            vec![Path::field("other")],
            IndexShape::default(),
        )
        .unwrap_err();
    assert!(matches!(error, Error::NameTaken { .. }), "{error}");

    // The same name on another table is a different index.
    catalog
        .create_index(
            carts.id,
            "by_id",
            vec![Path::field("id")],
            IndexShape {
                unique: true,
                search: false,
                spatial: false,
                vector: None,
            },
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn a_scan_returns_live_records_including_this_transactions_own_writes() {
    let (_backend, store) = store();
    let (namespace, database, table) = create_tree(&store, ("prod", "orders", "users"));
    let namespace = tessari_types::NamespaceId::new(namespace);
    let database = tessari_types::DatabaseId::new(database);
    let table = tessari_types::TableId::new(table);
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
    let namespace = tessari_types::NamespaceId::new(namespace);
    let database = tessari_types::DatabaseId::new(database);
    let table = tessari_types::TableId::new(table);
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
                kind: TableKind::Edge(None),
                ..TableShape::default()
            },
        )
        .unwrap();
    transaction.commit().unwrap();

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let mut indexed: Vec<Vec<Path>> = catalog
        .indexes_on(follows.id)
        .unwrap()
        .into_iter()
        .map(|index| index.fields)
        .collect();
    indexed.sort();
    assert_eq!(
        indexed,
        vec![vec![Path::field("in")], vec![Path::field("out")]],
        "an edge table needs both directions"
    );
    assert!(catalog.table(follows.id).unwrap().unwrap().is_edge());

    // And each endpoint is declared, so an edge table can also be schemafull
    // without the caller declaring fields the store itself fills in.
    let mut declared: Vec<(String, tessari_types::FieldKind)> = catalog
        .fields_on(follows.id)
        .unwrap()
        .into_iter()
        .map(|field| (field.name, field.kind))
        .collect();
    declared.sort();
    assert_eq!(
        declared,
        vec![
            ("in".to_owned(), tessari_types::FieldKind::Record),
            ("out".to_owned(), tessari_types::FieldKind::Record),
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
            .indexes_on(tessari_types::TableId::new(table))
            .unwrap()
            .is_empty()
    );
    assert!(
        catalog
            .fields_on(tessari_types::TableId::new(table))
            .unwrap()
            .is_empty()
    );
    assert!(
        !catalog
            .table(tessari_types::TableId::new(table))
            .unwrap()
            .unwrap()
            .is_edge()
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
                kind: TableKind::Edge(None),
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
                Value::Record(tessari_types::RecordRef::new(users.id, RecordId::Int(1))),
            ),
            (
                "in".to_owned(),
                Value::Record(tessari_types::RecordRef::new(users.id, RecordId::Int(2))),
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
    crate::replay(&store, &replica);

    let mut transaction = replica.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    assert!(catalog.table(follows.id).unwrap().unwrap().is_edge());
    assert_eq!(catalog.indexes_on(follows.id).unwrap().len(), 2);
    assert_eq!(catalog.fields_on(follows.id).unwrap().len(), 2);

    // And the entry the index holds is the same one, so a traversal on the
    // replica answers what it answers on the leader.
    let index = catalog
        .indexes_on(follows.id)
        .unwrap()
        .into_iter()
        .find(|index| index.fields == vec![Path::field("out")])
        .expect("an edge table has an index on out");
    let anchor = Value::Record(tessari_types::RecordRef::new(users.id, RecordId::Int(1)));
    assert_eq!(
        transaction
            .records_by_index(&index, &[anchor])
            .unwrap()
            .len(),
        1
    );
}

/// A table's own next record number, in one transaction.
fn next(store: &Store, table: u32) -> u64 {
    let mut transaction = store.begin().unwrap();
    let number = Catalog::new(&mut transaction)
        .next_record_number(tessari_types::TableId::new(table))
        .unwrap();
    transaction.commit().unwrap();
    number
}

#[test]
fn a_table_numbers_the_records_it_names_itself_from_one_upwards() {
    // One rather than zero: a record legitimately called `users:0` would spend
    // the rest of its life being read as an unset identity.
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "line_items"));

    assert_eq!(next(&store, table), 1);
    assert_eq!(next(&store, table), 2);
    assert_eq!(next(&store, table), 3);
}

#[test]
fn two_tables_count_their_records_independently() {
    // The reason there is a counter per table rather than one per store. A
    // shared counter would work and would also leave both tables full of gaps
    // that nothing in either of them explains.
    let (_backend, store) = store();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let users = catalog
        .create_table(namespace.id, database.id, "users", TableShape::default())
        .unwrap();
    let items = catalog
        .create_table(namespace.id, database.id, "items", TableShape::default())
        .unwrap();
    transaction.commit().unwrap();

    assert_eq!(next(&store, users.id.get()), 1);
    assert_eq!(next(&store, users.id.get()), 2);
    assert_eq!(next(&store, items.id.get()), 1);
    assert_eq!(next(&store, users.id.get()), 3);
    assert_eq!(next(&store, items.id.get()), 2);
}

#[test]
fn the_record_counter_does_not_regress_when_the_store_is_reopened() {
    // The failure this test exists for is silent in a way the others are not: a
    // counter that comes back lower re-issues an identity that already names a
    // record, and the next write under it *replaces* that record instead of
    // adding one. Nothing is in an error state, on either side, ever.
    let (backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "line_items"));

    assert_eq!(next(&store, table), 1);
    assert_eq!(next(&store, table), 2);
    drop(store);

    let reopened = Store::open(backend).unwrap();
    assert_eq!(next(&reopened, table), 3);
}

#[test]
fn a_record_number_that_was_never_committed_is_never_spent() {
    // The counter is written in the caller's transaction, so a write that rolls
    // back takes its identity with it. The alternative — a counter outside the
    // transaction — would leave a gap for every refused insert, which is
    // harmless right up until someone reads the gaps as deleted records.
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "line_items"));

    let mut abandoned = store.begin().unwrap();
    assert_eq!(
        Catalog::new(&mut abandoned)
            .next_record_number(tessari_types::TableId::new(table))
            .unwrap(),
        1
    );
    drop(abandoned);

    assert_eq!(next(&store, table), 1);
}

#[test]
fn two_transactions_racing_for_one_table_never_receive_the_same_number() {
    // The same mechanism that keeps names unique, and the reason this needs no
    // lock: both transactions read the counter and both *write* it, and conflict
    // detection is over writes.
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "line_items"));
    let id = tessari_types::TableId::new(table);

    let mut first = store.begin().unwrap();
    let mut second = store.begin().unwrap();
    let mine = Catalog::new(&mut first).next_record_number(id).unwrap();
    let theirs = Catalog::new(&mut second).next_record_number(id).unwrap();
    assert_eq!(mine, theirs, "both read the same counter, as they must");

    first.commit().unwrap();
    let error = second.commit().unwrap_err();
    assert!(matches!(error, Error::Conflict { .. }), "{error}");

    // And the loser's number was never spent, so the retry gets the next one
    // rather than the one it was already holding.
    assert_eq!(next(&store, table), 2);
}

#[test]
fn the_record_counter_reaches_a_replica_rather_than_being_derived_there() {
    // A counter is an ordinary catalog record (ADR-0009), so it travels in the
    // log like a definition does. A replica that derived its own would start at
    // one and hand out identities the leader has already given away.
    let (_backend, store) = store();
    let (_, _, table) = create_tree(&store, ("prod", "orders", "line_items"));
    assert_eq!(next(&store, table), 1);
    assert_eq!(next(&store, table), 2);

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    crate::replay(&store, &replica);

    assert_eq!(next(&replica, table), 3);
}
