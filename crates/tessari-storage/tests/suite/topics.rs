//! A topic travels by replay (G037 S6.1).
//!
//! Positions are decided in the writer's commit and derived again on each
//! apply, so a replica that derived them differently — or not at all — would
//! still hold every message and answer a read with the wrong positions. The
//! three topic keyspaces are therefore compared entry for entry, beside the
//! messages and the readers' stored positions.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use tessari_storage::Now;

use tessari_encoding::{KeyKind, encode_payload};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_storage::{Catalog, RecordAddress, Store, TableKind, TableShape, TopicDeclaration};
use tessari_types::{RecordId, Value};

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn entries(backend: &Arc<dyn KvBackend>, kind: KeyKind) -> Entries {
    backend
        .scan(&ScanRequest {
            keyspace: kind.keyspace(),
            range: KeyRange::prefix(&[kind.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .now()
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key.as_slice().to_vec(), value.as_slice().to_vec()))
        .collect()
}

#[test]
fn a_replica_holds_the_same_messages_positions_and_readers() {
    let source_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).now().unwrap();
    let mut transaction = source.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog.create_database(namespace.id, "app").now().unwrap();
    let topic = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            TableShape {
                kind: TableKind::Topic(TopicDeclaration::default()),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    transaction.commit().now().unwrap();
    let at = |id: &str| RecordAddress::new(namespace.id, database.id, topic.id, RecordId::from(id));
    // One message per commit, then two in one commit, so a replica that
    // numbered a commit's messages in another order would disagree.
    for batch in [&["m1"][..], &["m2"], &["m4", "m3"], &["m5"]] {
        let mut transaction = source.begin().now().unwrap();
        for id in batch {
            transaction.put(at(id), encode_payload(&Value::from(*id)).into_bytes());
        }
        transaction.commit().now().unwrap();
    }
    for (consumer, position) in [("audit", 2), ("mail", 5)] {
        let mut transaction = source.begin().now().unwrap();
        Catalog::new(&mut transaction).set_topic_position(
            namespace.id,
            database.id,
            topic.id,
            consumer,
            position,
        );
        transaction.commit().now().unwrap();
    }

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).now().unwrap();
    crate::replay(&source, &replica);

    let messages = |store: &Store| {
        store
            .begin()
            .now()
            .unwrap()
            .topic_after(namespace.id, database.id, topic.id, 0, 100)
            .now()
            .unwrap()
            .messages
            .into_iter()
            .map(|message| (message.position, message.id))
            .collect::<Vec<_>>()
    };
    let written = messages(&source);
    assert_eq!(
        written.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "the writer's own positions"
    );
    assert_eq!(messages(&replica), written);
    for kind in [
        KeyKind::TopicOffset,
        KeyKind::TopicEntry,
        KeyKind::TopicHead,
    ] {
        let held = entries(&source_backend, kind);
        assert!(!held.is_empty(), "{kind:?} holds nothing on the writer");
        assert_eq!(entries(&replica_backend, kind), held, "{kind:?}");
    }
    let readers = |store: &Store| {
        let mut transaction = store.begin().now().unwrap();
        Catalog::new(&mut transaction)
            .topic_positions(namespace.id, database.id, topic.id)
            .now()
            .unwrap()
    };
    assert_eq!(
        readers(&source),
        vec![("audit".to_owned(), 2), ("mail".to_owned(), 5)]
    );
    assert_eq!(readers(&replica), readers(&source));
}

/// A size-retained topic's evictions are deletes in the appending commit's log
/// record, so a replica that applies the log holds the same messages, the same
/// positions and the same byte count without deciding anything (G055 C8).
#[test]
fn a_replica_holds_what_a_size_retention_kept() {
    let source_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).now().unwrap();
    let mut transaction = source.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog.create_database(namespace.id, "app").now().unwrap();
    let topic = catalog
        .create_table(
            namespace.id,
            database.id,
            "sized",
            TableShape {
                kind: TableKind::Topic(TopicDeclaration {
                    retain_bytes: Some(64),
                    ..TopicDeclaration::default()
                }),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    transaction.commit().now().unwrap();
    for n in 1..=12 {
        let mut transaction = source.begin().now().unwrap();
        transaction.put(
            RecordAddress::new(namespace.id, database.id, topic.id, RecordId::Int(n)),
            encode_payload(&Value::from("a message of some length")).into_bytes(),
        );
        transaction.commit().now().unwrap();
    }
    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).now().unwrap();
    crate::replay(&source, &replica);

    let messages = |store: &Store| {
        store
            .begin()
            .now()
            .unwrap()
            .topic_after(namespace.id, database.id, topic.id, 0, 100)
            .now()
            .unwrap()
            .messages
            .into_iter()
            .map(|message| message.position)
            .collect::<Vec<_>>()
    };
    let kept = messages(&source);
    assert!(!kept.is_empty() && kept.len() < 12, "kept {kept:?}");
    assert_eq!(kept.last(), Some(&12));
    assert_eq!(messages(&replica), kept);
    for kind in [
        KeyKind::TopicOffset,
        KeyKind::TopicEntry,
        KeyKind::TopicHead,
        KeyKind::TopicBytes,
    ] {
        let held = entries(&source_backend, kind);
        assert!(!held.is_empty(), "{kind:?} holds nothing on the writer");
        assert_eq!(entries(&replica_backend, kind), held, "{kind:?}");
    }
    let bytes = |store: &Store| {
        store
            .begin()
            .now()
            .unwrap()
            .topic_bytes(namespace.id, database.id, topic.id)
            .now()
            .unwrap()
    };
    assert!(bytes(&source) <= 64);
    assert_eq!(bytes(&replica), bytes(&source));
}

fn wall_millis() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A store with one topic, and the address of a message in it.
fn one_topic() -> (
    Store,
    tessari_types::NamespaceId,
    tessari_types::DatabaseId,
    tessari_types::TableId,
) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(backend).now().unwrap();
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog.create_database(namespace.id, "app").now().unwrap();
    let topic = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            TableShape {
                kind: TableKind::Topic(TopicDeclaration::default()),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    transaction.commit().now().unwrap();
    (store, namespace.id, database.id, topic.id)
}

fn append(
    store: &Store,
    (namespace, database, topic): (
        tessari_types::NamespaceId,
        tessari_types::DatabaseId,
        tessari_types::TableId,
    ),
    ids: &[&str],
) {
    let mut transaction = store.begin().now().unwrap();
    for id in ids {
        transaction.put(
            RecordAddress::new(namespace, database, topic, RecordId::from(*id)),
            encode_payload(&Value::from(*id)).into_bytes(),
        );
    }
    transaction.commit().now().unwrap();
}

/// Wait, without sleeping, until the wall clock has passed `instant`.
fn past(instant: u64) {
    while wall_millis() <= instant {
        std::hint::spin_loop();
    }
}

/// Every message carries the instant its commit appended it, one instant per
/// commit, never earlier than the one before (ADR-0133 D3).
#[test]
fn a_message_carries_the_instant_it_was_appended() {
    let (store, namespace, database, topic) = one_topic();
    let before = wall_millis();
    append(&store, (namespace, database, topic), &["m1"]);
    let between = wall_millis();
    past(between);
    append(&store, (namespace, database, topic), &["m2", "m3"]);
    let after = wall_millis();
    let read = store
        .begin()
        .now()
        .unwrap()
        .topic_after(namespace, database, topic, 0, 100)
        .now()
        .unwrap();
    let instants: Vec<u64> = read
        .messages
        .iter()
        .map(|message| message.appended.unwrap())
        .collect();
    assert_eq!(instants.len(), 3);
    assert!(
        before <= instants[0] && instants[0] <= between,
        "{instants:?}"
    );
    assert!(
        between < instants[1] && instants[1] <= after,
        "{instants:?}"
    );
    assert_eq!(instants[1], instants[2], "one commit, one instant");
}

/// A clock already ahead of this node's — a leader before this one ran fast —
/// is kept rather than gone back on, so instants never decrease in a topic.
#[test]
fn a_topic_clock_never_goes_back() {
    let (store, namespace, database, topic) = one_topic();
    let ahead = wall_millis().saturating_add(3_600_000);
    let clocks = tessari_storage::SYSTEM_TABLES
        .iter()
        .find(|(_, name)| *name == "TOPIC_CLOCKS")
        .unwrap()
        .0;
    let mut transaction = store.begin().now().unwrap();
    transaction.put(
        RecordAddress::new(
            tessari_types::NamespaceId::new(0),
            tessari_types::DatabaseId::new(0),
            clocks,
            RecordId::Text(format!(
                "{}/{}/{}",
                namespace.get(),
                database.get(),
                topic.get()
            )),
        ),
        encode_payload(&Value::from(i64::try_from(ahead).unwrap())).into_bytes(),
    );
    transaction.commit().now().unwrap();
    append(&store, (namespace, database, topic), &["m1"]);
    let read = store
        .begin()
        .now()
        .unwrap()
        .topic_after(namespace, database, topic, 0, 100)
        .now()
        .unwrap();
    assert_eq!(read.messages[0].appended, Some(ahead));
}

/// `SINCE` answers the first position appended at or after an instant, and
/// nothing when every message is older (ADR-0133 D3).
#[test]
fn since_finds_the_first_message_at_or_after_an_instant() {
    let (store, namespace, database, topic) = one_topic();
    let since = |instant: u64| {
        store
            .begin()
            .now()
            .unwrap()
            .topic_since(namespace, database, topic, instant)
            .now()
            .unwrap()
    };
    assert_eq!(since(0), None, "an empty topic");
    let mut instants = Vec::new();
    for batch in [&["m1"][..], &["m2", "m3"], &["m4"], &["m5"]] {
        append(&store, (namespace, database, topic), batch);
        let read = store
            .begin()
            .now()
            .unwrap()
            .topic_after(namespace, database, topic, 0, 100)
            .now()
            .unwrap();
        let newest = read.messages.last().unwrap().appended.unwrap();
        instants.push(newest);
        past(newest);
    }
    assert_eq!(since(0), Some(1));
    assert_eq!(since(instants[0]), Some(1));
    assert_eq!(
        since(instants[0].saturating_add(1)),
        Some(2),
        "the first of a commit's two"
    );
    assert_eq!(since(instants[1]), Some(2));
    assert_eq!(since(instants[2]), Some(4));
    assert_eq!(since(instants[3]), Some(5));
    assert_eq!(since(instants[3].saturating_add(1)), None);
}

/// A store still holding the format before the instant keeps no clock and
/// answers no instant, so the build before it can still open the store
/// (ADR-0133 D3, ADR-0118).
#[test]
fn a_store_holding_an_older_format_keeps_no_append_instant() {
    use tessari_encoding::{FormatVersion, FormatVersionKey, StoreKey, StoreValue};
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    let mut transaction = store.begin().now().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").now().unwrap();
    let database = catalog.create_database(namespace.id, "app").now().unwrap();
    let topic = catalog
        .create_table(
            namespace.id,
            database.id,
            "events",
            TableShape {
                kind: TableKind::Topic(TopicDeclaration::default()),
                ..TableShape::default()
            },
        )
        .now()
        .unwrap();
    transaction.commit().now().unwrap();
    backend
        .apply(tessari_kv::WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(8).encode(),
        ))
        .now()
        .unwrap();
    append(&store, (namespace.id, database.id, topic.id), &["m1"]);
    let read = store
        .begin()
        .now()
        .unwrap()
        .topic_after(namespace.id, database.id, topic.id, 0, 100)
        .now()
        .unwrap();
    assert_eq!(read.messages.len(), 1);
    assert_eq!(read.messages[0].appended, None);
    let clocks = tessari_storage::SYSTEM_TABLES
        .iter()
        .find(|(_, name)| *name == "TOPIC_CLOCKS")
        .unwrap()
        .0;
    let held = store
        .begin()
        .now()
        .unwrap()
        .get(&RecordAddress::new(
            tessari_types::NamespaceId::new(0),
            tessari_types::DatabaseId::new(0),
            clocks,
            RecordId::Text(format!(
                "{}/{}/{}",
                namespace.id.get(),
                database.id.get(),
                topic.id.get()
            )),
        ))
        .now()
        .unwrap();
    assert_eq!(held, None, "no clock record on an older format");
}
