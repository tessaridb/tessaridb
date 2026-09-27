//! A catalog row read once is not read again until the catalog changes.
//!
//! Asserted on the backend reads a resolution costs, which is the whole content
//! of the mechanism, and on the one thing it must never do: answer a reader with
//! a row the catalog no longer holds at that reader's snapshot, or withhold one
//! it still does.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value, WriteBatch,
};
use tessari_types::{DatabaseId, NamespaceId, TableId};

use crate::catalog::{Catalog, TableShape};
use crate::store::Store;

/// A memory backend that counts every read it answers.
#[derive(Debug)]
struct Counting {
    engine: MemoryBackend,
    reads: AtomicUsize,
}

impl Counting {
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn read(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        self.read();
        self.engine.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        self.read();
        self.engine.scan(request)
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        self.read();
        self.engine.first_of_each(keyspace, ranges)
    }

    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        self.read();
        self.engine.count(keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.engine.apply(batch)
    }
}

struct Fixture {
    backend: Arc<Counting>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
}

fn fixture() -> Fixture {
    let backend = Arc::new(Counting {
        engine: MemoryBackend::new(),
        reads: AtomicUsize::new(0),
    });
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("ns").unwrap().id;
    let database = catalog.create_database(namespace, "db").unwrap().id;
    let table = catalog
        .create_table(namespace, database, "t", TableShape::default())
        .unwrap()
        .id;
    transaction.commit().unwrap();
    Fixture {
        backend,
        store,
        namespace,
        database,
        table,
    }
}

/// What a statement naming `ns.db.t` asks the catalog.
fn resolve(fixture: &Fixture) -> Option<TableId> {
    let mut transaction = fixture.store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("ns").unwrap()?;
    let database = catalog.database_id(namespace, "db").unwrap()?;
    let table = catalog.table_id(namespace, database, "t").unwrap()?;
    catalog
        .table(table)
        .unwrap()
        .map(|definition| definition.id)
}

#[test]
fn a_name_resolved_once_is_not_read_again() {
    let fixture = fixture();
    assert_eq!(resolve(&fixture), Some(fixture.table));
    let before = fixture.backend.reads();
    assert_eq!(resolve(&fixture), Some(fixture.table));
    let catalog_reads = fixture.backend.reads() - before;
    // Beginning a transaction reads the committed version; nothing else may.
    let _transaction = fixture.store.begin().unwrap();
    let begin_reads = fixture.backend.reads() - before - catalog_reads;
    assert!(
        catalog_reads <= begin_reads,
        "a second resolution read the backend {catalog_reads} times; beginning costs {begin_reads}"
    );
}

#[test]
fn a_dropped_table_is_gone_for_new_readers_and_kept_for_old_ones() {
    let fixture = fixture();
    assert_eq!(resolve(&fixture), Some(fixture.table), "warm");
    let mut before_the_drop = fixture.store.begin().unwrap();

    let mut transaction = fixture.store.begin().unwrap();
    assert!(
        Catalog::new(&mut transaction)
            .drop_table(fixture.table)
            .unwrap()
    );
    transaction.commit().unwrap();

    assert_eq!(
        resolve(&fixture),
        None,
        "a reader after the drop was served the dropped table"
    );
    let old = Catalog::new(&mut before_the_drop);
    assert_eq!(
        old.table_id(fixture.namespace, fixture.database, "t")
            .unwrap(),
        Some(fixture.table),
        "a reader from before the drop lost the table its snapshot holds"
    );
}
