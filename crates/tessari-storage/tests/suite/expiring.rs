//! The expiry index travels by replay, like every structure derived from the
//! log (G035 S2.1).
//!
//! A follower that applied expiring records without writing their index entries
//! would answer correctly — reads judge each version against the clock — and
//! would never remove anything once promoted. Nothing would be in an error state,
//! so the entries are compared themselves.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::{ExpiryKey, KeyKind, StoreKey};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_storage::{RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

/// An instant an hour from now, on the transaction's own clock.
fn later(store: &Store) -> u64 {
    store.begin().unwrap().clock().saturating_add(3_600_000)
}

fn at(id: &str) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(id),
    )
}

fn entries(backend: &Arc<dyn KvBackend>) -> Vec<ExpiryKey> {
    backend
        .scan(&ScanRequest {
            keyspace: KeyKind::ExpiryIndex.keyspace(),
            range: KeyRange::prefix(&[KeyKind::ExpiryIndex.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap()
        .into_iter()
        .map(|(key, _)| ExpiryKey::decode(key.as_slice()).unwrap())
        .collect()
}

#[test]
fn a_replica_holds_the_same_expiry_entries_as_the_node_that_wrote_them() {
    let source_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).unwrap();
    let first = later(&source);
    for (id, instant) in [("a", first), ("b", first.saturating_add(1)), ("c", 0)] {
        let mut transaction = source.begin().unwrap();
        transaction.put(at(id), b"v".to_vec());
        if instant > 0 {
            transaction.expire_pending(&at(id), instant);
        }
        transaction.commit().unwrap();
    }
    // Moved, so the source holds one entry for `a` and not two.
    let mut transaction = source.begin().unwrap();
    transaction.put(at("a"), b"v".to_vec());
    transaction.expire_pending(&at("a"), first.saturating_add(7));
    transaction.commit().unwrap();

    let written = entries(&source_backend);
    assert_eq!(
        written
            .iter()
            .map(|entry| (entry.id.clone(), entry.at))
            .collect::<Vec<_>>(),
        vec![
            (RecordId::from("b"), first.saturating_add(1)),
            (RecordId::from("a"), first.saturating_add(7)),
        ],
        "one entry per expiring record, at its current instant, instant-ordered"
    );

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    crate::replay(&source, &replica);
    assert_eq!(entries(&replica_backend), written);
}
