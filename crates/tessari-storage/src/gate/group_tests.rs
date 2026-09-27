//! Commits that arrive while one is being landed land together.
//!
//! Asserted on the number of engine writes, which is the whole content of the
//! mechanism: a backend whose first group write is held open lets eight more
//! commits arrive behind it. Landed one at a time — each waiting for the turn
//! while the one before it is written — nothing is ever staged and the count
//! is nine; landed as a group it is two.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value, WriteBatch,
};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use crate::{RecordAddress, Store};

const BEHIND: usize = 8;

/// A memory backend whose first group write waits until it is let go.
#[derive(Debug)]
struct HeldOpen {
    engine: MemoryBackend,
    writes: AtomicUsize,
    released: Mutex<bool>,
    let_go: Condvar,
}

impl HeldOpen {
    fn new() -> Self {
        Self {
            engine: MemoryBackend::new(),
            writes: AtomicUsize::new(0),
            released: Mutex::new(false),
            let_go: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.let_go.notify_all();
    }
}

impl KvBackend for HeldOpen {
    fn name(&self) -> &'static str {
        "held-open"
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
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.engine.apply(batch)
    }

    fn groups_writes(&self) -> bool {
        true
    }

    fn apply_group(&self, batches: Vec<WriteBatch>) -> (usize, Result<()>) {
        if self.writes.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.let_go.wait(released).unwrap();
            }
        }
        self.engine.apply_group(batches)
    }
}

fn address(id: usize) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(format!("r{id}").as_str()),
    )
}

fn write(store: &Store, id: usize) {
    let mut transaction = store.begin().unwrap();
    transaction.put(address(id), format!("{{\"n\":{id}}}").into_bytes());
    transaction.commit().unwrap();
}

#[test]
fn commits_arriving_while_one_lands_land_together() {
    let backend = Arc::new(HeldOpen::new());
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    // Opening the store writes; only the commits below are counted.
    backend.writes.store(0, Ordering::SeqCst);

    std::thread::scope(|scope| {
        let first = scope.spawn(|| write(&store, 0));
        let behind: Vec<_> = (1..=BEHIND)
            .map(|id| {
                let store = &store;
                scope.spawn(move || write(store, id))
            })
            .collect();
        // Every commit behind the first has derived and staged while the first
        // is held on its way to the engine; bounded, so a regression fails here
        // rather than hanging.
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.write_gate().staged() < BEHIND {
            assert!(
                Instant::now() < deadline,
                "only {} of {BEHIND} commits staged behind the one landing",
                store.write_gate().staged()
            );
            std::thread::yield_now();
        }
        backend.release();
        first.join().unwrap();
        for writer in behind {
            writer.join().unwrap();
        }
    });

    assert_eq!(
        backend.writes.load(Ordering::SeqCst),
        2,
        "nine commits should land in two engine writes"
    );
    let transaction = store.begin().unwrap();
    for id in 0..=BEHIND {
        assert_eq!(
            transaction.get(&address(id)).unwrap(),
            Some(format!("{{\"n\":{id}}}").into_bytes()),
            "record {id}"
        );
    }
}
