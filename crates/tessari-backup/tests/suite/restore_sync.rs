//! A restore lands its records and syncs per chunk, not per record (Q-899).
//!
//! Nothing acknowledges a restore part-way — the caller learns the outcome when
//! `read` returns — so a record owes durability only by then. Syncing each one
//! cost a device flush per record (~5 ms on macOS at `PowerLossSafe`), which is
//! the cost a follower's round already stopped paying (G054 W8e).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_kv::{Key, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value, WriteBatch};
use tessari_session::Session;
use tessari_storage::Store;

/// A memory backend that counts how each write lands. The counts are
/// statistics read on the test's own thread, so `Relaxed` is all they need.
#[derive(Debug)]
struct Counted {
    inner: MemoryBackend,
    synced: AtomicUsize,
    unsynced: AtomicUsize,
    syncs: AtomicUsize,
    /// Writes landed unsynced since the last sync: what a power loss would take.
    exposed: AtomicUsize,
}

impl KvBackend for Counted {
    fn name(&self) -> &'static str {
        "counted"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> tessari_kv::Result<Option<Value>> {
        self.inner.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> tessari_kv::Result<Vec<(Key, Value)>> {
        self.inner.scan(request)
    }

    fn apply(&self, batch: WriteBatch) -> tessari_kv::Result<()> {
        self.synced.fetch_add(1, Ordering::Relaxed);
        self.inner.apply(batch)
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> tessari_kv::Result<()> {
        self.unsynced.fetch_add(1, Ordering::Relaxed);
        self.exposed.fetch_add(1, Ordering::Relaxed);
        self.inner.apply(batch)
    }

    fn sync_applied(&self) -> tessari_kv::Result<()> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.exposed.store(0, Ordering::Relaxed);
        Ok(())
    }

    // As a power-loss-safe engine does, so the store reaches it through the
    // same view of staged writes production does.
    fn groups_writes(&self) -> bool {
        true
    }
}

impl Counted {
    /// (synced writes, unsynced writes, syncs) so far.
    fn landed(&self) -> (usize, usize, usize) {
        (
            self.synced.load(Ordering::Relaxed),
            self.unsynced.load(Ordering::Relaxed),
            self.syncs.load(Ordering::Relaxed),
        )
    }
}

/// How many records the source writes: more than two chunks and not a
/// multiple of one, so a last partial chunk exists to be synced.
const RECORDS: usize = 2_500;

fn backup_of_many() -> Vec<u8> {
    let source = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = Session::new(&source);
    session
        .run("DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; DEFINE TABLE t SCHEMALESS;")
        .unwrap();
    for at in 0..RECORDS {
        session
            .run(&format!("CREATE t:{at} = {{ n: {at} }};"))
            .unwrap();
    }
    let mut taken = Vec::new();
    tessari_backup::write(&source, &mut taken).unwrap();
    taken
}

fn counted() -> (Arc<Counted>, Store) {
    let backend = Arc::new(Counted {
        inner: MemoryBackend::new(),
        synced: AtomicUsize::new(0),
        unsynced: AtomicUsize::new(0),
        syncs: AtomicUsize::new(0),
        exposed: AtomicUsize::new(0),
    });
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    (backend, store)
}

#[test]
fn a_restore_lands_each_record_unsynced_and_syncs_per_chunk() {
    let taken = backup_of_many();
    let (backend, restored) = counted();
    let (synced, unsynced, syncs) = backend.landed();

    let outcome = tessari_backup::read(&restored, &mut taken.as_slice()).unwrap();

    let records = usize::try_from(outcome.records).unwrap();
    assert!(records >= RECORDS, "the backup held {records} records");
    let (synced_after, unsynced_after, syncs_after) = backend.landed();
    assert_eq!(synced_after, synced, "a restored record paid its own sync");
    assert!(unsynced_after - unsynced >= records);
    let chunk = usize::try_from(tessari_backup::RESTORE_CHUNK).unwrap();
    let bound = records.div_ceil(chunk) + 1;
    let paid = syncs_after - syncs;
    assert!(
        (records / chunk..=bound).contains(&paid),
        "{paid} syncs for {records} records, bound {bound}"
    );
    assert_eq!(
        backend.exposed.load(Ordering::Relaxed),
        0,
        "the restore returned with records not yet durable"
    );
}

#[test]
fn a_restore_refused_part_way_syncs_what_it_applied() {
    let mut taken = backup_of_many();
    // A byte flipped near the end damages one late record; everything before
    // it is applied, and must be durable when the refusal is returned.
    let at = taken.len() - 8;
    taken[at] ^= 0xff;
    let (backend, restored) = counted();
    let (_, unsynced, syncs) = backend.landed();

    assert!(tessari_backup::read(&restored, &mut taken.as_slice()).is_err());

    let (_, unsynced_after, syncs_after) = backend.landed();
    assert!(
        unsynced_after > unsynced,
        "nothing was applied before the damage"
    );
    assert!(syncs_after > syncs, "what was applied was left unsynced");
    assert_eq!(
        backend.exposed.load(Ordering::Relaxed),
        0,
        "the refusal returned with records not yet durable"
    );
}
