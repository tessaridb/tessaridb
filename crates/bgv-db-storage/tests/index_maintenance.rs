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
