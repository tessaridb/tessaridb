//! The record layout, exercised through a real backend.
//!
//! The unit tests prove that encoded bytes compare in the intended order. This
//! one proves the consequence that actually matters: a snapshot read is a seek
//! plus taking the first entry, and it lands on the newest version at or before
//! the snapshot without scanning past the record it asked for.

#![allow(clippy::unwrap_used)]

use std::ops::Bound;

use tessari_encoding::{RecordKey, RecordValue, StoreKey, StoreValue};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

const NAMESPACE: NamespaceId = NamespaceId::new(1);
const DATABASE: DatabaseId = DatabaseId::new(1);
const TABLE: TableId = TableId::new(1);

fn record_key(id: &RecordId, version: u64) -> RecordKey {
    RecordKey::new(
        NAMESPACE,
        DATABASE,
        TABLE,
        id.clone(),
        Sequence::new(version),
    )
}

/// Write one version of one record.
fn write_version(backend: &MemoryBackend, id: &RecordId, version: u64, value: &RecordValue) {
    let key = record_key(id, version);
    backend
        .apply(WriteBatch::new().put(RecordKey::keyspace(), key.encode(), value.encode()))
        .unwrap();
}

/// The read a transaction performs: seek to the snapshot, take the first entry.
fn read_at_snapshot(
    backend: &MemoryBackend,
    id: &RecordId,
    snapshot: u64,
) -> Option<(Sequence, RecordValue)> {
    let versions_prefix = RecordKey::versions_prefix(NAMESPACE, DATABASE, TABLE, id);
    let bounds = KeyRange::prefix(&versions_prefix);
    let request = ScanRequest {
        keyspace: RecordKey::keyspace(),
        range: KeyRange::from_bounds(
            Bound::Included(record_key(id, snapshot).encode()),
            bounds.end().clone(),
        ),
        direction: ScanDirection::Forward,
        limit: Some(1),
    };
    let found = backend.scan(&request).unwrap();
    found.first().map(|(key, value)| {
        let decoded_key = RecordKey::decode(key.as_slice()).unwrap();
        let decoded_value = RecordValue::decode(value.as_slice()).unwrap();
        (decoded_key.version, decoded_value)
    })
}

#[test]
fn versions_of_one_record_scan_newest_first() {
    let backend = MemoryBackend::new();
    let id = RecordId::from("subject");
    for version in [1_u64, 5, 9] {
        write_version(
            &backend,
            &id,
            version,
            &RecordValue::Present(format!("v{version}").into_bytes()),
        );
    }

    let request = ScanRequest {
        keyspace: RecordKey::keyspace(),
        range: KeyRange::prefix(&RecordKey::versions_prefix(NAMESPACE, DATABASE, TABLE, &id)),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let versions: Vec<u64> = backend
        .scan(&request)
        .unwrap()
        .iter()
        .map(|(key, _)| RecordKey::decode(key.as_slice()).unwrap().version.get())
        .collect();

    assert_eq!(versions, vec![9, 5, 1]);
}

#[test]
fn a_snapshot_read_lands_on_the_newest_version_at_or_before_it() {
    let backend = MemoryBackend::new();
    let id = RecordId::from("subject");
    for version in [1_u64, 5, 9] {
        write_version(
            &backend,
            &id,
            version,
            &RecordValue::Present(format!("v{version}").into_bytes()),
        );
    }

    for (snapshot, expected) in [(1_u64, 1_u64), (4, 1), (5, 5), (8, 5), (9, 9), (100, 9)] {
        let (version, value) = read_at_snapshot(&backend, &id, snapshot).unwrap();
        assert_eq!(version.get(), expected, "snapshot {snapshot}");
        assert_eq!(value.payload(), format!("v{expected}").as_bytes());
    }
}

#[test]
fn a_snapshot_before_the_first_version_sees_nothing() {
    let backend = MemoryBackend::new();
    let id = RecordId::from("subject");
    write_version(&backend, &id, 5, &RecordValue::Present(b"v5".to_vec()));

    assert!(read_at_snapshot(&backend, &id, 4).is_none());
    assert!(read_at_snapshot(&backend, &id, 0).is_none());
}

#[test]
fn a_deletion_is_visible_as_a_tombstone_and_older_versions_survive_it() {
    let backend = MemoryBackend::new();
    let id = RecordId::from("subject");
    write_version(&backend, &id, 2, &RecordValue::Present(b"alive".to_vec()));
    write_version(&backend, &id, 7, &RecordValue::Tombstone);

    let (before, value) = read_at_snapshot(&backend, &id, 6).unwrap();
    assert_eq!(before.get(), 2);
    assert_eq!(value.payload(), b"alive");

    let (after, value) = read_at_snapshot(&backend, &id, 7).unwrap();
    assert_eq!(after.get(), 7);
    assert!(
        value.is_tombstone(),
        "a delete must be a version, not an absence"
    );
}

#[test]
fn a_read_never_runs_past_the_record_it_asked_for() {
    let backend = MemoryBackend::new();
    // Two record ids where one is a prefix of the other — the case a missing
    // component terminator would collapse.
    let short = RecordId::from("a");
    let long = RecordId::from("ab");
    write_version(
        &backend,
        &short,
        1,
        &RecordValue::Present(b"short".to_vec()),
    );
    write_version(&backend, &long, 1, &RecordValue::Present(b"long".to_vec()));

    // A snapshot far beyond either version: without a bounded range and a
    // terminated id, this would return the neighbouring record.
    let (_, value) = read_at_snapshot(&backend, &short, u64::MAX).unwrap();
    assert_eq!(value.payload(), b"short");
    let (_, value) = read_at_snapshot(&backend, &long, u64::MAX).unwrap();
    assert_eq!(value.payload(), b"long");
}

#[test]
fn records_of_different_tables_never_appear_in_one_table_scan() {
    let backend = MemoryBackend::new();
    let id = RecordId::from("same-id-in-both");
    for table in [1_u32, 2] {
        let key = RecordKey::new(
            NAMESPACE,
            DATABASE,
            TableId::new(table),
            id.clone(),
            Sequence::new(1),
        );
        backend
            .apply(WriteBatch::new().put(
                RecordKey::keyspace(),
                key.encode(),
                RecordValue::Present(vec![u8::try_from(table).unwrap()]).encode(),
            ))
            .unwrap();
    }

    let request = ScanRequest {
        keyspace: RecordKey::keyspace(),
        range: KeyRange::prefix(&RecordKey::table_prefix(
            NAMESPACE,
            DATABASE,
            TableId::new(1),
        )),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let found = backend.scan(&request).unwrap();
    assert_eq!(found.len(), 1);
    let decoded = RecordKey::decode(found[0].0.as_slice()).unwrap();
    assert_eq!(decoded.table, TableId::new(1));
}
