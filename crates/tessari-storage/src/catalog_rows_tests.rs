//! A catalog row read once is not read again until the catalog changes.
//!
//! Asserted on the backend reads a resolution costs, which is the whole content
//! of the mechanism, and on the one thing it must never do: answer a reader with
//! a row the catalog no longer holds at that reader's snapshot, or withhold one
//! it still does.

#![allow(clippy::unwrap_used)]

use crate::now::Now;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_kv::KvFuture;
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value, WriteBatch,
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

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        Box::pin(async move {
            self.read();
            self.engine.get(keyspace, key).await
        })
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        Box::pin(async move {
            self.read();
            self.engine.scan(request).await
        })
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        Box::pin(async move {
            self.read();
            self.engine.first_of_each(keyspace, ranges).await
        })
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        Box::pin(async move {
            self.read();
            self.engine.count(keyspace, range).await
        })
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        Box::pin(async move { self.engine.apply(batch).await })
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
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("ns").now().unwrap().id;
    let database = catalog.create_database(namespace, "db").now().unwrap().id;
    let table = catalog
        .create_table(namespace, database, "t", TableShape::default())
        .now()
        .unwrap()
        .id;
    transaction.commit().now().unwrap();
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
    let mut transaction = fixture.store.begin().now().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("ns").now().unwrap()?;
    let database = catalog.database_id(namespace, "db").now().unwrap()?;
    let table = catalog.table_id(namespace, database, "t").now().unwrap()?;
    catalog
        .table(table)
        .now()
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
    let _transaction = fixture.store.begin().now().unwrap();
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
    let mut before_the_drop = fixture.store.begin().now().unwrap();

    let mut transaction = fixture.store.begin().now().unwrap();
    assert!(
        Catalog::new(&mut transaction)
            .drop_table(fixture.table)
            .now()
            .unwrap()
    );
    transaction.commit().now().unwrap();

    assert_eq!(
        resolve(&fixture),
        None,
        "a reader after the drop was served the dropped table"
    );
    let old = Catalog::new(&mut before_the_drop);
    assert_eq!(
        old.table_id(fixture.namespace, fixture.database, "t")
            .now()
            .unwrap(),
        Some(fixture.table),
        "a reader from before the drop lost the table its snapshot holds"
    );
}
