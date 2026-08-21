//! Index entries are kept in step with the records, and they are *derived*.
//!
//! Nothing about an index travels in the log. A replica computes the same
//! entries from the same record mutations and the same catalog, which is what
//! the last test here checks — and it is why the log did not need a new shape.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bgv_db_encoding::{
    IndexAddress, KeyKind, StoreKey, StoreValue, UniqueIndexKey, encode_payload,
};
use bgv_db_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use bgv_db_storage::{Catalog, Error, IndexDefinition, RecordAddress, Store};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value};

struct Fixture {
    backend: Arc<dyn KvBackend>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    index: IndexDefinition,
}

impl Fixture {
    /// A namespace, a database, a table and one index on `email`, all committed.
    ///
    /// The index is committed before any record is written, because maintenance
    /// reads the catalog as of the committed state and a backfill is not built.
    fn new(unique: bool) -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(Arc::clone(&backend)).unwrap();

        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "orders").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "users")
            .unwrap();
        let index = catalog
            .create_index(table.id, "by_email", vec!["email".to_owned()], unique)
            .unwrap();
        transaction.commit().unwrap();

        Self {
            backend,
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            index,
        }
    }

    fn at(&self, id: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(id),
        )
    }

    fn record(email: Option<Value>) -> Value {
        let mut fields = BTreeMap::from([("name".to_owned(), Value::from("ada"))]);
        if let Some(value) = email {
            fields.insert("email".to_owned(), value);
        }
        Value::Object(fields)
    }

    fn write(&self, id: &str, email: Option<Value>) -> Result<Sequence, Error> {
        let mut transaction = self.store.begin().unwrap();
        transaction.put(
            self.at(id),
            encode_payload(&Self::record(email)).into_bytes(),
        );
        transaction.commit()
    }

    fn delete(&self, id: &str) {
        let mut transaction = self.store.begin().unwrap();
        transaction.delete(self.at(id));
        transaction.commit().unwrap();
    }

    fn address(&self) -> IndexAddress {
        IndexAddress::new(self.namespace, self.database, self.table, self.index.id)
    }

    /// Every entry of the index, read straight out of the substrate.
    fn entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let kind = if self.index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = self.address().prefix(kind);
        let request = ScanRequest {
            keyspace: kind.keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        self.backend
            .scan(&request)
            .unwrap()
            .into_iter()
            .map(|(key, value)| (key.as_slice().to_vec(), value.as_slice().to_vec()))
            .collect()
    }
}

#[test]
fn writing_a_record_writes_its_index_entry() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("ada@example.com")))
        .unwrap();
    assert_eq!(fixture.entries().len(), 1);
}

#[test]
fn changing_the_indexed_value_leaves_no_entry_behind() {
    // An orphan entry is the failure mode secondary indexes are known for: it
    // points at a record that no longer holds that value, and nothing ever
    // reconciles it.
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("old@example.com")))
        .unwrap();
    fixture
        .write("u1", Some(Value::from("new@example.com")))
        .unwrap();

    let entries = fixture.entries();
    assert_eq!(entries.len(), 1, "the old entry must be gone");
}

#[test]
fn deleting_a_record_removes_its_entry() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("ada@example.com")))
        .unwrap();
    fixture.delete("u1");
    assert!(fixture.entries().is_empty());
}

#[test]
fn a_record_missing_the_indexed_field_is_not_indexed() {
    // `none` means the field is not there, so there is no value to place.
    // Indexing it as `none` would make every such record collide in a unique
    // index — a constraint nobody asked for.
    let fixture = Fixture::new(true);
    fixture.write("u1", None).unwrap();
    fixture.write("u2", None).unwrap();
    assert!(fixture.entries().is_empty());
}

#[test]
fn a_record_whose_field_is_null_is_indexed_under_null() {
    // `null` is a value. Two records holding it collide in a unique index the
    // same way two records holding one email would.
    let fixture = Fixture::new(true);
    fixture.write("u1", Some(Value::Null)).unwrap();
    assert_eq!(fixture.entries().len(), 1);

    let error = fixture.write("u2", Some(Value::Null)).unwrap_err();
    assert!(matches!(error, Error::UniqueViolation { .. }), "{error}");
}

#[test]
fn a_non_unique_index_holds_two_records_under_one_value() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("shared@example.com")))
        .unwrap();
    fixture
        .write("u2", Some(Value::from("shared@example.com")))
        .unwrap();
    assert_eq!(fixture.entries().len(), 2);
}

#[test]
fn a_unique_index_refuses_a_second_record_with_the_same_value() {
    let fixture = Fixture::new(true);
    fixture
        .write("u1", Some(Value::from("one@example.com")))
        .unwrap();

    let error = fixture
        .write("u2", Some(Value::from("one@example.com")))
        .unwrap_err();
    assert!(matches!(error, Error::UniqueViolation { .. }), "{error}");
    assert_eq!(error.code(), "validation");
    assert!(
        !error.is_retryable(),
        "the caller's data violates a constraint"
    );

    assert_eq!(fixture.entries().len(), 1, "the refused write left nothing");
}

#[test]
fn a_unique_index_lets_the_same_record_be_rewritten() {
    let fixture = Fixture::new(true);
    fixture
        .write("u1", Some(Value::from("one@example.com")))
        .unwrap();
    fixture
        .write("u1", Some(Value::from("one@example.com")))
        .unwrap();
    assert_eq!(fixture.entries().len(), 1);
}

#[test]
fn two_records_claiming_one_unique_value_in_one_transaction_are_refused() {
    // A precondition cannot catch this: both find the key absent, both are
    // satisfied, and the second would silently overwrite the first.
    let fixture = Fixture::new(true);
    let mut transaction = fixture.store.begin().unwrap();
    let payload =
        encode_payload(&Fixture::record(Some(Value::from("one@example.com")))).into_bytes();
    transaction.put(fixture.at("u1"), payload.clone());
    transaction.put(fixture.at("u2"), payload);

    let error = transaction.commit().unwrap_err();
    assert!(matches!(error, Error::UniqueViolation { .. }), "{error}");
    assert!(fixture.entries().is_empty(), "nothing was written");
}

#[test]
fn a_unique_entry_points_at_the_record_that_holds_the_value() {
    let fixture = Fixture::new(true);
    fixture
        .write("u1", Some(Value::from("ada@example.com")))
        .unwrap();

    let entries = fixture.entries();
    let (key, value) = entries.first().expect("one entry");
    let decoded = UniqueIndexKey::decode(key).unwrap();
    assert_eq!(decoded.address, fixture.address());
    assert_eq!(
        <UniqueIndexKey as StoreKey>::Value::decode(value)
            .unwrap()
            .id,
        RecordId::from("u1")
    );
}

#[test]
fn a_replica_derives_the_same_entries_from_the_same_log() {
    // Nothing about the index travels in the log. This is what makes that safe.
    let source = Fixture::new(false);
    source
        .write("u1", Some(Value::from("ada@example.com")))
        .unwrap();
    source
        .write("u2", Some(Value::from("grace@example.com")))
        .unwrap();
    source
        .write("u1", Some(Value::from("ada2@example.com")))
        .unwrap();

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    for (sequence, record) in source.store.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    let mirrored = Fixture {
        backend: replica_backend,
        store: replica,
        namespace: source.namespace,
        database: source.database,
        table: source.table,
        index: source.index.clone(),
    };
    assert_eq!(mirrored.entries(), source.entries());
    assert_eq!(source.entries().len(), 2);
}

#[test]
fn a_unique_lookup_returns_the_record_that_holds_the_value() {
    let fixture = Fixture::new(true);
    fixture
        .write("u1", Some(Value::from("ada@example.com")))
        .unwrap();
    fixture
        .write("u2", Some(Value::from("grace@example.com")))
        .unwrap();

    let transaction = fixture.store.begin().unwrap();
    let found = transaction
        .records_by_index(&fixture.index, &[Value::from("ada@example.com")])
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, RecordId::from("u1"));

    assert!(
        transaction
            .records_by_index(&fixture.index, &[Value::from("nobody@example.com")])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_non_unique_lookup_returns_every_record_holding_the_value() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("shared@example.com")))
        .unwrap();
    fixture
        .write("u2", Some(Value::from("shared@example.com")))
        .unwrap();
    fixture
        .write("u3", Some(Value::from("other@example.com")))
        .unwrap();

    let transaction = fixture.store.begin().unwrap();
    let found = transaction
        .records_by_index(&fixture.index, &[Value::from("shared@example.com")])
        .unwrap();
    let ids: Vec<String> = found.iter().map(|(id, _)| id.to_string()).collect();
    assert_eq!(ids, vec!["u1".to_owned(), "u2".to_owned()]);
}

#[test]
fn a_lookup_never_returns_a_record_that_no_longer_holds_the_value() {
    // The reader began before the change, so the entry it finds is the *new*
    // one. Confirming the candidate at the reader's own snapshot is what keeps
    // that entry from producing a row that does not match.
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("old@example.com")))
        .unwrap();

    let reader = fixture.store.begin().unwrap();
    fixture
        .write("u1", Some(Value::from("new@example.com")))
        .unwrap();

    let wrong = reader
        .records_by_index(&fixture.index, &[Value::from("new@example.com")])
        .unwrap();
    assert!(
        wrong.is_empty(),
        "at this snapshot the record still holds the old value"
    );

    // And the honest limitation: the old entry is gone, so the old value finds
    // nothing either. Sound, not complete.
    let missed = reader
        .records_by_index(&fixture.index, &[Value::from("old@example.com")])
        .unwrap();
    assert!(missed.is_empty(), "the entry for the old value was removed");

    // A reader at the latest committed state is exact.
    let current = fixture.store.begin().unwrap();
    let found = current
        .records_by_index(&fixture.index, &[Value::from("new@example.com")])
        .unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn a_lookup_sees_this_transactions_own_uncommitted_writes() {
    // Entries are derived at commit, so without this the writer could not find
    // what it had just written through the index it declared.
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("shared@example.com")))
        .unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    transaction.put(
        fixture.at("u2"),
        encode_payload(&Fixture::record(Some(Value::from("shared@example.com")))).into_bytes(),
    );
    // And one that moves away from the value must stop matching.
    transaction.put(
        fixture.at("u1"),
        encode_payload(&Fixture::record(Some(Value::from("moved@example.com")))).into_bytes(),
    );

    let found = transaction
        .records_by_index(&fixture.index, &[Value::from("shared@example.com")])
        .unwrap();
    let ids: Vec<String> = found.iter().map(|(id, _)| id.to_string()).collect();
    assert_eq!(ids, vec!["u2".to_owned()]);
}

#[test]
fn a_deleted_record_is_not_returned_by_a_lookup() {
    let fixture = Fixture::new(false);
    fixture
        .write("u1", Some(Value::from("ada@example.com")))
        .unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    transaction.delete(fixture.at("u1"));
    assert!(
        transaction
            .records_by_index(&fixture.index, &[Value::from("ada@example.com")])
            .unwrap()
            .is_empty()
    );
}

/// A table with rows, and an index declared on it *afterwards*.
fn indexed_after_the_fact(unique: bool) -> Fixture {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();

    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, "users")
        .unwrap();
    transaction.commit().unwrap();

    let bare = Fixture {
        backend: Arc::clone(&backend),
        store,
        namespace: namespace.id,
        database: database.id,
        table: table.id,
        // Placeholder; the real definition is created below, after the rows.
        index: IndexDefinition {
            id: bgv_db_types::IndexId::new(0),
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            name: "placeholder".to_owned(),
            fields: vec!["email".to_owned()],
            unique,
        },
    };
    bare.write("u1", Some(Value::from("ada@example.com")))
        .unwrap();
    bare.write("u2", Some(Value::from("grace@example.com")))
        .unwrap();
    bare.write("u3", None).unwrap();

    let mut transaction = bare.store.begin().unwrap();
    let index = Catalog::new(&mut transaction)
        .create_index(bare.table, "by_email", vec!["email".to_owned()], unique)
        .unwrap();
    transaction.commit().unwrap();

    Fixture { index, ..bare }
}

#[test]
fn an_index_declared_after_the_rows_holds_nothing_until_it_is_backfilled() {
    let fixture = indexed_after_the_fact(false);
    assert!(
        fixture.entries().is_empty(),
        "maintenance only sees mutations"
    );

    let indexed = fixture.store.backfill_index(&fixture.index).unwrap();
    assert_eq!(indexed, 2, "the record with no email is not in the index");
    assert_eq!(fixture.entries().len(), 2);

    let transaction = fixture.store.begin().unwrap();
    let found = transaction
        .records_by_index(&fixture.index, &[Value::from("ada@example.com")])
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, RecordId::from("u1"));
}

#[test]
fn a_backfill_is_idempotent_and_composes_with_maintenance() {
    let fixture = indexed_after_the_fact(false);
    assert_eq!(fixture.store.backfill_index(&fixture.index).unwrap(), 2);
    assert_eq!(fixture.store.backfill_index(&fixture.index).unwrap(), 2);
    assert_eq!(fixture.entries().len(), 2);

    // A write after the backfill is maintained normally, and one that changes a
    // value leaves no entry behind.
    fixture
        .write("u4", Some(Value::from("new@example.com")))
        .unwrap();
    fixture
        .write("u1", Some(Value::from("moved@example.com")))
        .unwrap();
    assert_eq!(fixture.entries().len(), 3);
}

#[test]
fn a_backfill_refuses_a_unique_index_two_existing_rows_already_violate() {
    let fixture = indexed_after_the_fact(true);
    // `u1` already holds this address, so the index cannot become unique.
    fixture
        .write("u4", Some(Value::from("ada@example.com")))
        .unwrap();
    let before = fixture.entries();

    let error = fixture.store.backfill_index(&fixture.index).unwrap_err();
    assert!(matches!(error, Error::UniqueViolation { .. }), "{error}");
    assert_eq!(
        fixture.entries(),
        before,
        "a refused backfill writes nothing at all"
    );
}

#[test]
fn a_unique_index_does_not_constrain_rows_it_has_not_been_backfilled_over() {
    // The hazard worth naming: until the backfill runs, the rows that predate
    // the index have no entries, so maintenance has nothing to collide with and
    // accepts a duplicate of one of them. The constraint begins to hold when the
    // backfill succeeds — and the backfill is what refuses to let it "begin"
    // over data that already violates it.
    let fixture = indexed_after_the_fact(true);
    fixture
        .write("u4", Some(Value::from("ada@example.com")))
        .expect("accepted, because u1's entry does not exist yet");

    assert_eq!(fixture.entries().len(), 1, "only the new row is indexed");
    assert!(fixture.store.backfill_index(&fixture.index).is_err());
}
