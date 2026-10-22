//! The expiry index travels by replay, like every structure derived from the
//! log (G035 S2.1).
//!
//! A follower that applied expiring records without writing their index entries
//! would answer correctly — reads judge each version against the clock — and
//! would never remove anything once promoted. Nothing would be in an error state,
//! so the entries are compared themselves.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use tessari_storage::Now;

use tessari_encoding::{ExpiryKey, KeyKind, StoreKey};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_storage::{RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

/// An instant an hour from now, on the transaction's own clock.
fn later(store: &Store) -> u64 {
    store
        .begin()
        .now()
        .unwrap()
        .clock()
        .saturating_add(3_600_000)
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
        .now()
        .unwrap()
        .into_iter()
        .map(|(key, _)| ExpiryKey::decode(key.as_slice()).unwrap())
        .collect()
}

#[test]
fn a_replica_holds_the_same_expiry_entries_as_the_node_that_wrote_them() {
    let source_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).now().unwrap();
    let first = later(&source);
    for (id, instant) in [("a", first), ("b", first.saturating_add(1)), ("c", 0)] {
        let mut transaction = source.begin().now().unwrap();
        transaction.put(at(id), b"v".to_vec());
        if instant > 0 {
            transaction.expire_pending(&at(id), instant);
        }
        transaction.commit().now().unwrap();
    }
    // Moved, so the source holds one entry for `a` and not two.
    let mut transaction = source.begin().now().unwrap();
    transaction.put(at("a"), b"v".to_vec());
    transaction.expire_pending(&at("a"), first.saturating_add(7));
    transaction.commit().now().unwrap();

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
    let replica = Store::open(Arc::clone(&replica_backend)).now().unwrap();
    crate::replay(&source, &replica);
    assert_eq!(entries(&replica_backend), written);
}

/// G036 S4.1: evictions reach a replica as the deletes the writer's commit
/// carried, so the replayed space holds the same keys and the same
/// modified-order entries.
#[test]
fn a_replica_of_a_limited_space_holds_what_the_writer_kept() {
    use tessari_encoding::ModifiedKey;
    use tessari_storage::{Catalog, Eviction, SpaceDeclaration, SpaceLimit, TableKind, TableShape};

    let source_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).now().unwrap();
    let mut transaction = source.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog.create_database(namespace.id, "app").now().unwrap();
    let space = catalog
        .create_table(
            namespace.id,
            database.id,
            "lru",
            TableShape {
                kind: TableKind::Space(SpaceDeclaration {
                    limit: Some(SpaceLimit {
                        max: 3,
                        eviction: Eviction::Modified,
                    }),
                    memory: false,
                }),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    transaction.commit().now().unwrap();
    let at = |id: &str| RecordAddress::new(namespace.id, database.id, space.id, RecordId::from(id));
    for id in ["a", "b", "c", "d", "e", "b"] {
        let mut transaction = source.begin().now().unwrap();
        transaction.put(at(id), b"v".to_vec());
        transaction.commit().now().unwrap();
    }

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).now().unwrap();
    crate::replay(&source, &replica);

    let held = |store: &Store| {
        store
            .begin()
            .now()
            .unwrap()
            .scan_table(namespace.id, database.id, space.id)
            .now()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    };
    let order = |backend: &Arc<dyn KvBackend>| {
        backend
            .scan(&ScanRequest {
                keyspace: KeyKind::ModifiedOrder.keyspace(),
                range: KeyRange::prefix(&[KeyKind::ModifiedOrder.tag()]),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .now()
            .unwrap()
            .into_iter()
            .map(|(key, _)| ModifiedKey::decode(key.as_slice()).unwrap().id)
            .collect::<Vec<_>>()
    };
    let kept = vec![
        RecordId::from("b"),
        RecordId::from("d"),
        RecordId::from("e"),
    ];
    assert_eq!(held(&source), kept);
    assert_eq!(held(&replica), kept);
    assert_eq!(
        order(&source_backend),
        vec![
            RecordId::from("d"),
            RecordId::from("e"),
            RecordId::from("b")
        ]
    );
    assert_eq!(order(&replica_backend), order(&source_backend));
}

/// The fields of a hash, as the store holds its payload.
fn hash(fields: &[&str]) -> Vec<u8> {
    let object = fields
        .iter()
        .map(|field| ((*field).to_owned(), tessari_types::Value::from(1_i64)))
        .collect();
    tessari_encoding::encode_payload(&tessari_types::Value::Object(object))
        .as_slice()
        .to_vec()
}

/// The field names a read answers, or `None` for no record.
fn fields_read(store: &Store, address: &RecordAddress) -> Option<Vec<String>> {
    let payload = store.begin().now().unwrap().get(address).now().unwrap()?;
    match tessari_encoding::decode_payload(&payload).unwrap() {
        tessari_types::Value::Object(fields) => Some(fields.into_keys().collect()),
        other => Some(vec![format!("{other:?}")]),
    }
}

/// A transaction whose clock reads past `instant`, begun without waiting on
/// anything but the millisecond itself.
fn after_instant(store: &Store, instant: u64) -> tessari_storage::Transaction<'_> {
    loop {
        let transaction = store.begin().now().unwrap();
        if transaction.clock() > instant {
            return transaction;
        }
    }
}

#[test]
fn a_hash_field_given_an_instant_keeps_it_until_its_field_is_written() {
    let store = Store::open(Arc::new(MemoryBackend::new())).now().unwrap();
    let key = at("h");
    let far = later(&store);
    let mut transaction = store.begin().now().unwrap();
    transaction.put(key.clone(), hash(&["a", "b"]));
    transaction.expire_fields_pending(&key, vec![("a".to_owned(), far)]);
    assert_eq!(
        transaction.field_expiry(&key).now().unwrap(),
        [("a".to_owned(), far)]
    );
    transaction.commit().now().unwrap();
    let mut reading = store.begin().now().unwrap();
    assert_eq!(
        reading.field_expiry(&key).now().unwrap(),
        [("a".to_owned(), far)]
    );
    // A plain write replaces the version, its fields' instants included —
    // the stored ones, and ones this transaction gave before writing again.
    let mut transaction = store.begin().now().unwrap();
    transaction.put(key.clone(), hash(&["a", "b"]));
    assert!(transaction.field_expiry(&key).now().unwrap().is_empty());
    transaction.expire_fields_pending(&key, vec![("b".to_owned(), far)]);
    transaction.put(key.clone(), hash(&["a", "b"]));
    assert!(transaction.field_expiry(&key).now().unwrap().is_empty());
    transaction.commit().now().unwrap();
    assert!(
        store
            .begin()
            .now()
            .unwrap()
            .field_expiry(&key)
            .now()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_hash_field_past_its_instant_is_not_read_and_the_last_takes_the_key() {
    let store = Store::open(Arc::new(MemoryBackend::new())).now().unwrap();
    let key = at("h");
    let mut transaction = store.begin().now().unwrap();
    let soon = transaction.clock().saturating_add(1);
    transaction.put(key.clone(), hash(&["a", "b"]));
    transaction.expire_fields_pending(&key, vec![("a".to_owned(), soon)]);
    transaction.commit().now().unwrap();
    let mut reading = after_instant(&store, soon);
    assert!(reading.field_expiry(&key).now().unwrap().is_empty());
    drop(reading);
    assert_eq!(fields_read(&store, &key), Some(vec!["b".to_owned()]));
    // An instant already past, given in the writing transaction, removes the
    // field there; the last field removed so takes the key with it.
    let mut transaction = store.begin().now().unwrap();
    let now = transaction.clock();
    transaction.put(key.clone(), hash(&["b"]));
    transaction.expire_fields_pending(&key, vec![("b".to_owned(), now)]);
    assert_eq!(transaction.get(&key).now().unwrap(), None);
    transaction.commit().now().unwrap();
    assert_eq!(fields_read(&store, &key), None);
}
