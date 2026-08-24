//! The record store, unchanged, on the persistent backend.
//!
//! The layer above was written against the contract and not against the
//! in-memory backend. These tests are how that stops being a claim: the same
//! guarantees and the same permitted anomalies, asserted against data on disk.
//!
//! They are a subset — the exhaustive isolation suite lives with the storage
//! crate, and duplicating it here would mean two copies to keep in step. What is
//! re-asserted here is what could plausibly differ on a real engine: version
//! visibility across a snapshot, conflict detection, and the fact that neither
//! is confused by a restart.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::KvBackend;
use tessari_lsm::{Durability, LsmBackend, StoreConfig};
use tessari_storage::{Error, RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

fn open(path: &std::path::Path) -> Store {
    let backend = LsmBackend::open(path, StoreConfig::new(Durability::ProcessCrashSafe)).unwrap();
    Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
}

fn at(id: &str) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(id),
    )
}

fn write(store: &Store, id: &str, payload: &[u8]) {
    let mut transaction = store.begin().unwrap();
    transaction.put(at(id), payload.to_vec());
    transaction.commit().unwrap();
}

#[test]
fn a_held_snapshot_still_reads_the_old_version_from_disk() {
    let root = tempfile::tempdir().unwrap();
    let store = open(&root.path().join("store"));
    write(&store, "r", b"first");

    let reader = store.begin().unwrap();
    write(&store, "r", b"second");

    assert_eq!(reader.get(&at("r")).unwrap(), Some(b"first".to_vec()));
    assert_eq!(
        store.begin().unwrap().get(&at("r")).unwrap(),
        Some(b"second".to_vec())
    );
}

#[test]
fn a_deletion_hides_the_record_from_later_snapshots_and_not_from_earlier_ones() {
    let root = tempfile::tempdir().unwrap();
    let store = open(&root.path().join("store"));
    write(&store, "r", b"alive");

    let reader = store.begin().unwrap();
    let mut deleter = store.begin().unwrap();
    deleter.delete(at("r"));
    deleter.commit().unwrap();

    assert_eq!(reader.get(&at("r")).unwrap(), Some(b"alive".to_vec()));
    assert_eq!(store.begin().unwrap().get(&at("r")).unwrap(), None);
}

#[test]
fn the_first_committer_still_wins_when_the_versions_are_on_disk() {
    let root = tempfile::tempdir().unwrap();
    let store = open(&root.path().join("store"));
    write(&store, "r", b"base");

    let mut first = store.begin().unwrap();
    let mut second = store.begin().unwrap();
    first.put(at("r"), b"first".to_vec());
    second.put(at("r"), b"second".to_vec());

    first.commit().unwrap();
    let error = second.commit().unwrap_err();
    assert!(matches!(error, Error::Conflict { .. }), "{error}");

    assert_eq!(
        store.begin().unwrap().get(&at("r")).unwrap(),
        Some(b"first".to_vec())
    );
}

#[test]
fn versions_and_the_committed_position_come_back_together_after_a_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let tail = {
        let store = open(&path);
        write(&store, "r", b"first");
        write(&store, "r", b"second");
        write(&store, "other", b"value");
        store.committed_tail().unwrap()
    };

    let reopened = open(&path);
    assert_eq!(reopened.committed_tail().unwrap(), tail);

    let transaction = reopened.begin().unwrap();
    assert_eq!(
        transaction.get(&at("r")).unwrap(),
        Some(b"second".to_vec()),
        "a reopened store must read the newest version, not the first one it finds"
    );
    assert_eq!(
        transaction.get(&at("other")).unwrap(),
        Some(b"value".to_vec())
    );
}

#[test]
fn a_log_replayed_between_two_stores_on_disk_reproduces_them_byte_for_byte() {
    // The log lives above the substrate, not inside a backend. This is what
    // that claim costs to verify: the same replay, on the durable engine.
    use tessari_kv::{Key, KeyRange, Keyspace, ScanRequest};
    use tessari_types::Sequence;

    let root = tempfile::tempdir().unwrap();
    let source_backend = LsmBackend::open(
        root.path().join("source"),
        StoreConfig::new(Durability::ProcessCrashSafe),
    )
    .unwrap();
    let source_backend = Arc::new(source_backend) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).unwrap();

    write(&source, "alpha", b"one");
    write(&source, "beta", b"two");
    write(&source, "alpha", b"one-again");
    let mut remover = source.begin().unwrap();
    remover.delete(at("beta"));
    remover.commit().unwrap();

    let replica_backend = LsmBackend::open(
        root.path().join("replica"),
        StoreConfig::new(Durability::ProcessCrashSafe),
    )
    .unwrap();
    let replica_backend = Arc::new(replica_backend) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();

    for (sequence, record) in source.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    // The node identity is the one key a replay must **not** reproduce. It lives
    // in `META` precisely so that it does not travel (ADR-0018 §1): a replica
    // that came up holding the source's id would be a second process claiming
    // one identity, which is the failure that split exists to prevent. So it is
    // excluded here and asserted to differ below — a hole on its own would also
    // cover the key going missing entirely.
    let node_identity = Key::from(vec![0x38]);
    for keyspace in Keyspace::ALL {
        let request = ScanRequest::new(*keyspace, KeyRange::all());
        let derived = |backend: &Arc<dyn KvBackend>| {
            backend
                .scan(&request)
                .unwrap()
                .into_iter()
                .filter(|(key, _)| *key != node_identity)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            derived(&source_backend),
            derived(&replica_backend),
            "keyspace {keyspace} differs after replay onto disk"
        );
    }
    assert_ne!(
        source.node_identity().unwrap().id,
        replica.node_identity().unwrap().id,
        "the replica came up holding the source's identity"
    );
}

#[test]
fn a_commit_after_a_reopen_continues_the_sequence_instead_of_restarting_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let before = {
        let store = open(&path);
        write(&store, "r", b"value");
        store.committed_tail().unwrap()
    };

    let reopened = open(&path);
    let mut transaction = reopened.begin().unwrap();
    transaction.put(at("r"), b"after".to_vec());
    let after = transaction.commit().unwrap();

    assert!(
        after > before,
        "a restarted store handed out sequence {after}, at or behind {before}"
    );
}
