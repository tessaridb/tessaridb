//! A topic travels by replay (G037 S6.1).
//!
//! Positions are decided in the writer's commit and derived again on each
//! apply, so a replica that derived them differently — or not at all — would
//! still hold every message and answer a read with the wrong positions. The
//! three topic keyspaces are therefore compared entry for entry, beside the
//! messages and the readers' stored positions.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

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
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key.as_slice().to_vec(), value.as_slice().to_vec()))
        .collect()
}

#[test]
fn a_replica_holds_the_same_messages_positions_and_readers() {
    let source_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let source = Store::open(Arc::clone(&source_backend)).unwrap();
    let mut transaction = source.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "app").unwrap();
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
        .unwrap();
    transaction.commit().unwrap();
    let at = |id: &str| RecordAddress::new(namespace.id, database.id, topic.id, RecordId::from(id));
    // One message per commit, then two in one commit, so a replica that
    // numbered a commit's messages in another order would disagree.
    for batch in [&["m1"][..], &["m2"], &["m4", "m3"], &["m5"]] {
        let mut transaction = source.begin().unwrap();
        for id in batch {
            transaction.put(at(id), encode_payload(&Value::from(*id)).into_bytes());
        }
        transaction.commit().unwrap();
    }
    for (consumer, position) in [("audit", 2), ("mail", 5)] {
        let mut transaction = source.begin().unwrap();
        Catalog::new(&mut transaction).set_topic_position(
            namespace.id,
            database.id,
            topic.id,
            consumer,
            position,
        );
        transaction.commit().unwrap();
    }

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    crate::replay(&source, &replica);

    let messages = |store: &Store| {
        store
            .begin()
            .unwrap()
            .topic_after(namespace.id, database.id, topic.id, 0, 100)
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
        let mut transaction = store.begin().unwrap();
        Catalog::new(&mut transaction)
            .topic_positions(namespace.id, database.id, topic.id)
            .unwrap()
    };
    assert_eq!(
        readers(&source),
        vec![("audit".to_owned(), 2), ("mail".to_owned(), 5)]
    );
    assert_eq!(readers(&replica), readers(&source));
}
