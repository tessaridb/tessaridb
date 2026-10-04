//! A write the engine could not make durable stops the store (G059 C4).
//!
//! After a failed sync the bytes it covered may be gone while the engine's
//! cache marks them clean, so a later sync that succeeds proves nothing about
//! them — and a log with a hole in it recovers only up to the hole, taking the
//! commits acknowledged after it along. So the first such failure stops this
//! store taking writes until it is reopened and recovers from its log, even if
//! the device answers the next write. A refusal that took no write — busy, a
//! conflict, a bad request — does not stop it.
//!
//! The backend here fails one write on command and then works again, which is
//! exactly the case a store that merely reported the error would get wrong. It
//! is run twice: landing one batch at a time, and landing staged commits as a
//! group, which are the two paths every commit takes to the engine.

// bgv-allow(unwrap): test-only module; a panic is how a test reports its failure, as in group_tests.rs.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use tessari_kv::{
    Error, Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value,
    WriteBatch,
};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use crate::{RecordAddress, Store};

/// A memory backend that fails the next write when told to.
#[derive(Debug)]
struct Faulty {
    engine: MemoryBackend,
    next_failure: Mutex<Option<Error>>,
    grouped: bool,
}

impl Faulty {
    fn new(grouped: bool) -> Arc<Self> {
        Arc::new(Self {
            engine: MemoryBackend::new(),
            next_failure: Mutex::new(None),
            grouped,
        })
    }

    fn fail_next(&self, failure: Error) {
        *self.next_failure.lock().unwrap() = Some(failure);
    }
}

impl KvBackend for Faulty {
    fn name(&self) -> &'static str {
        "faulty"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        self.engine.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        self.engine.scan(request)
    }

    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        self.engine.count(keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        if let Some(failure) = self.next_failure.lock().unwrap().take() {
            return Err(failure);
        }
        self.engine.apply(batch)
    }

    fn groups_writes(&self) -> bool {
        self.grouped
    }
}

fn address(id: u32) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(format!("r{id}").as_str()),
    )
}

fn write(store: &Store, id: u32) -> crate::Result<()> {
    let mut transaction = store.begin()?;
    transaction.put(address(id), format!("{{\"n\":{id}}}").into_bytes());
    transaction.commit().map(|_| ())
}

fn read(store: &Store, id: u32) -> Option<Vec<u8>> {
    store.begin().unwrap().get(&address(id)).unwrap()
}

fn a_failed_sync_stops_writes_until_the_store_is_reopened(grouped: bool) {
    let backend = Faulty::new(grouped);
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    write(&store, 1).unwrap();

    backend.fail_next(Error::Unavailable {
        backend: "faulty",
        reason: "fsync of the write-ahead log failed".to_owned(),
    });
    let failed = write(&store, 2).unwrap_err();
    assert_eq!(failed.code(), "unavailable", "{failed}");

    // The device answers again; the store must not believe it.
    let refused = write(&store, 3).unwrap_err();
    assert_eq!(refused.code(), "lifecycle", "{refused}");
    assert!(
        refused.to_string().contains("stopped taking writes"),
        "{refused}"
    );
    assert!(
        refused
            .to_string()
            .contains("fsync of the write-ahead log failed"),
        "the refusal names what stopped it: {refused}"
    );
    assert!(read(&store, 1).is_some(), "reads still answer");
    assert!(read(&store, 3).is_none(), "the refused write did not land");

    drop(store);
    let reopened = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    write(&reopened, 4).unwrap();
    assert!(read(&reopened, 1).is_some() && read(&reopened, 4).is_some());
}

#[test]
fn a_failed_write_stops_a_store_that_lands_one_batch_at_a_time() {
    a_failed_sync_stops_writes_until_the_store_is_reopened(false);
}

#[test]
fn a_failed_write_stops_a_store_that_lands_commits_as_a_group() {
    a_failed_sync_stops_writes_until_the_store_is_reopened(true);
}

#[test]
fn a_write_refused_before_it_was_taken_does_not_stop_the_store() {
    for grouped in [false, true] {
        let backend = Faulty::new(grouped);
        let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
        backend.fail_next(Error::Busy {
            backend: "faulty",
            reason: "writes are stalled".to_owned(),
        });
        let busy = write(&store, 1).unwrap_err();
        assert_eq!(busy.code(), "busy", "{busy}");
        write(&store, 2).unwrap();
        assert!(read(&store, 2).is_some());
    }
}
