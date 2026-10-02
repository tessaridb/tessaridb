//! A follower's round lands its records and pays ONE sync (G054 W8e).
//!
//! The ask that follows a round is the follower's acknowledgement (ADR-0106
//! D5, D6), so every record a round applied must be durable before the round
//! answers — and only once, or a round of N records costs N device syncs and a
//! write waiting for a majority waits for all of them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_encoding::{LogRecord, Mutation, RecordValue, StampedValue};
use tessari_kv::{Key, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value, WriteBatch};
use tessari_storage::{Error, Horizon, Page, Store};
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, Sequence, TableId};

/// A memory backend that counts how each write lands. The counts are
/// statistics read on the test's own thread, so `Relaxed` is all they need.
#[derive(Debug)]
struct Counted {
    inner: MemoryBackend,
    synced: AtomicUsize,
    unsynced: AtomicUsize,
    syncs: AtomicUsize,
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
        self.inner.apply(batch)
    }

    fn sync_applied(&self) -> tessari_kv::Result<()> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    // As a power-loss-safe engine does, so the store reaches it through the
    // same view of staged writes production does — a view that once dropped
    // both calls above and synced every record anyway.
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

fn mutation(id: &str) -> Mutation {
    Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table: TableId::new(1),
        id: RecordId::from(id),
        shard: None,
        value: StampedValue::new(RecordValue::Present(b"v".to_vec())),
    }
}

fn records(positions: &[u64]) -> Vec<(Sequence, LogRecord)> {
    positions
        .iter()
        .map(|at| {
            (
                Sequence::new(*at),
                LogRecord::at(Epoch::new(7), vec![mutation(&format!("record-{at}"))]),
            )
        })
        .collect()
}

fn opened() -> (Arc<Counted>, Store) {
    let backend = Arc::new(Counted {
        inner: MemoryBackend::new(),
        synced: AtomicUsize::new(0),
        unsynced: AtomicUsize::new(0),
        syncs: AtomicUsize::new(0),
    });
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    (backend, store)
}

fn page<'a>(store: &Store, records: &'a [(Sequence, LogRecord)]) -> Page<'a> {
    Page {
        log: store.own_log(crate::FIXTURE_HOME).unwrap(),
        from: Sequence::new(1),
        previous: Epoch::ZERO,
        records,
        horizon: Horizon::Unstated,
    }
}

#[test]
fn a_round_of_records_lands_each_unsynced_and_syncs_once() {
    let (backend, store) = opened();
    let (synced, unsynced, syncs) = backend.landed();
    let sent = records(&[1, 2, 3, 4, 5]);

    let reached = store.apply_in_writer_order(&[page(&store, &sent)]).unwrap();

    assert_eq!(reached, vec![Some(Sequence::new(5))]);
    let (synced_after, unsynced_after, syncs_after) = backend.landed();
    assert_eq!(
        synced_after, synced,
        "a record of the round paid its own sync"
    );
    assert_eq!(unsynced_after, unsynced + 5);
    assert_eq!(
        syncs_after,
        syncs + 1,
        "the round did not sync exactly once"
    );
}

#[test]
fn a_round_refused_part_way_still_syncs_what_it_applied() {
    let (backend, store) = opened();
    let (_, _, syncs) = backend.landed();
    // Position 4 after 2 is a gap: the first two apply, the third is refused.
    let sent = records(&[1, 2, 4]);

    let refused = store.apply_in_writer_order(&[page(&store, &sent)]);

    assert!(matches!(refused, Err(Error::LogGap { .. })), "{refused:?}");
    let log = store.own_log(crate::FIXTURE_HOME).unwrap();
    assert_eq!(store.committed_tail(log).unwrap(), Sequence::new(2));
    assert_eq!(
        backend.landed().2,
        syncs + 1,
        "the records applied before the refusal were left unsynced"
    );
}
