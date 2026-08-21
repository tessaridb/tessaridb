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

use bgv_db_kv::KvBackend;
use bgv_db_lsm::{Durability, LsmBackend, StoreConfig};
use bgv_db_storage::{Error, RecordAddress, Store};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, TableId};

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
