//! Topics without a broker (G037), on both backends.
//!
//! Every case runs on the memory backend and on the disk one, for the reason
//! the key-value suite beside this one does.

mod consumers;
mod declaring;
mod order;
mod public;
mod retention;

use std::collections::BTreeMap;

use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_session::{Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

use super::key_value::{Backend, run};

/// A session in `prod`/`app` with a topic `events` declared.
pub(super) fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE app; USE DATABASE app;\n\
             DEFINE TOPIC events;",
        )
        .unwrap();
    session
}

/// A second session on the same store, in the same database.
pub(super) fn another(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    session
}

/// What a `READ FROM` answered: each message's position and identity, in the
/// order answered, and the notes.
pub(super) fn read(session: &mut Session<'_>, script: &str) -> (Vec<(u64, RecordId)>, Vec<Note>) {
    match run(session, script) {
        Outcome::Records { records, notes, .. } => (
            records
                .into_iter()
                .map(|(id, body)| {
                    let Value::Object(fields) = body else {
                        panic!("a message answered {body:?}");
                    };
                    let Some(Value::Number(Number::Integer(position))) = fields.get("position")
                    else {
                        panic!("no position in {fields:?}");
                    };
                    (u64::try_from(*position).unwrap(), id)
                })
                .collect(),
            notes,
        ),
        other => panic!("{script} answered {other:?}"),
    }
}

/// Every entry of both position indexes, decoded: position → message, and
/// message → position.
pub(super) fn entries(backend: &Backend) -> (Vec<(u64, RecordId)>, BTreeMap<RecordId, u64>) {
    use tessari_encoding::{KeyKind, StoreKey, TopicEntryKey, TopicOffsetKey};
    let scan = |kind: KeyKind| {
        backend
            .raw
            .scan(&ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::prefix(&[kind.tag()]),
                direction: ScanDirection::Forward,
                limit: None,
            })
            .unwrap()
    };
    let by_position = scan(KeyKind::TopicOffset)
        .into_iter()
        .map(|(key, _)| {
            let entry = TopicOffsetKey::decode(key.as_slice()).unwrap();
            (entry.offset, entry.id)
        })
        .collect();
    let mut by_message = BTreeMap::new();
    for (key, _) in scan(KeyKind::TopicEntry) {
        let entry = TopicEntryKey::decode(key.as_slice()).unwrap();
        assert!(
            by_message.insert(entry.id.clone(), entry.offset).is_none(),
            "{}: {:?} is filed twice by identity",
            backend.name,
            entry.id
        );
    }
    (by_position, by_message)
}
