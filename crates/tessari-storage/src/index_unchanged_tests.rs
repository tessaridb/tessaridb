//! An update that leaves every field an index reads as it was writes nothing to it.
//!
//! The entries such an update would delete and write back are the ones already
//! there, so the work — and for a full-text index, analysing the text twice — buys
//! nothing. Asserted on the batch `maintain` builds, where the difference is the
//! whole content of the code, and with a control arm that moves an indexed field.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{LogRecord, Mutation, RecordValue, StampedValue, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, Path, RecordId, TableId, Value};

use crate::catalog::{Catalog, IndexShape, TableShape};
use crate::index::maintain;
use crate::store::Store;
use crate::transaction::RecordAddress;

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
}

impl Fixture {
    /// A table with an ordered, a unique and a multi-valued index, holding one
    /// record written through a commit.
    fn new() -> Self {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("ns").unwrap().id;
        let database = catalog.create_database(namespace, "db").unwrap().id;
        let table = catalog
            .create_table(namespace, database, "t", TableShape::default())
            .unwrap()
            .id;
        catalog
            .create_index(
                table,
                "by_city",
                vec![Path::field("city")],
                IndexShape::default(),
            )
            .unwrap();
        catalog
            .create_index(
                table,
                "by_email",
                vec![Path::field("email")],
                IndexShape {
                    unique: true,
                    ..IndexShape::default()
                },
            )
            .unwrap();
        catalog
            .create_index(
                table,
                "by_tag",
                vec![Path::parse("tags[*]").expect("a path")],
                IndexShape::default(),
            )
            .unwrap();
        transaction.commit().unwrap();
        let fixture = Self {
            store,
            namespace,
            database,
            table,
        };
        let mut transaction = fixture.store.begin().unwrap();
        transaction.put(fixture.address(), fixture.record("a", Value::from(1_i64)));
        transaction.commit().unwrap();
        fixture
    }

    fn address(&self) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from("r"),
        )
    }

    fn record(&self, name: &str, city: Value) -> Vec<u8> {
        let fields = BTreeMap::from([
            ("name".to_owned(), Value::from(name)),
            ("city".to_owned(), city),
            ("email".to_owned(), Value::from("e@example.com")),
            (
                "tags".to_owned(),
                Value::Array(vec![Value::from("x"), Value::from("y")]),
            ),
        ]);
        encode_payload(&Value::Object(fields)).into_bytes()
    }

    /// The index writes an update of the record to `payload` implies.
    fn batch_for(&self, payload: Vec<u8>) -> WriteBatch {
        let update = LogRecord::new(vec![Mutation {
            namespace: self.namespace,
            database: self.database,
            table: self.table,
            id: RecordId::from("r"),
            shard: None,
            value: StampedValue::new(RecordValue::Present(payload)),
        }]);
        maintain(&self.store, &update, WriteBatch::new()).unwrap()
    }
}

#[test]
fn an_update_of_an_unindexed_field_writes_no_index_entry() {
    let fixture = Fixture::new();
    let batch = fixture.batch_for(fixture.record("b", Value::from(1_i64)));
    assert!(
        batch.is_empty(),
        "{} writes and {} preconditions for an update no index reads",
        batch.ops().len(),
        batch.preconditions().len()
    );
}

#[test]
fn an_update_of_an_indexed_field_still_moves_its_entry() {
    // The control arm: the skip must not swallow a change it should see.
    let fixture = Fixture::new();
    let batch = fixture.batch_for(fixture.record("a", Value::from(2_i64)));
    assert!(!batch.is_empty(), "moving `city` wrote no index entry");
}

#[test]
fn a_value_written_differently_is_a_change_even_when_it_compares_equal() {
    // `1` and `1.0` are one value to a comparison and two to an encoder, so an
    // index entry built from one is not proven to be the entry of the other.
    let fixture = Fixture::new();
    let batch = fixture.batch_for(fixture.record("a", Value::from(1.0_f64)));
    assert!(
        !batch.is_empty(),
        "a re-typed indexed value wrote no index entry"
    );
}
