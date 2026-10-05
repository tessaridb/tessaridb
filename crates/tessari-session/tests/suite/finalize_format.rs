//! A store keeps the format it held until an operator finalizes it, so the
//! build before can take it back (ADR-0118, G065 C2–C3).

use std::sync::Arc;
use std::time::Duration;

use tessari_encoding::{FormatVersion, FormatVersionKey, NodeVersion, StoreKey, StoreValue};
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::{Lease, Store};
use tessari_types::{Epoch, Number, Value};

/// A peer's id, never this store's.
const PEER: [u8; 16] = [0xaa; 16];
const PEER_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn stamp(backend: &Arc<dyn KvBackend>) -> u32 {
    let held = backend
        .get(FormatVersionKey::keyspace(), &FormatVersionKey.encode())
        .unwrap()
        .unwrap();
    FormatVersion::decode(held.as_slice()).unwrap().get()
}

fn stamped(backend: &Arc<dyn KvBackend>, version: u32) {
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(version).encode(),
        ))
        .unwrap();
}

/// A store as a format-5 build left it, opened by this one.
fn held_at_five() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    drop(Store::open(Arc::clone(&backend)).unwrap());
    stamped(&backend, 5);
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

fn answered_format(outcomes: &[Outcome]) -> i64 {
    match outcomes.last() {
        Some(Outcome::Value(Value::Object(fields))) => match fields.get("format") {
            Some(Value::Number(Number::Integer(format))) => *format,
            other => panic!("no format in the answer: {other:?}"),
        },
        other => panic!("not a value: {other:?}"),
    }
}

const SCHEMA: &str = "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d;\n\
     DEFINE COLLECTION notes; CREATE notes:1 = { doc: { a: 1 } };";

#[test]
fn a_finalize_raises_the_format_once_and_the_newer_value_is_then_written() {
    let (backend, store) = held_at_five();
    let mut session = Session::new(&store);
    session.run(SCHEMA).unwrap();
    assert!(matches!(
        session.run("DEFINE INDEX by_doc ON notes FIELDS doc CONTAINS;"),
        Err(Error::FormatNotFinalized { .. })
    ));

    let finalized = session.run("ALTER STORE FINALIZE FORMAT;").unwrap();
    let current = i64::from(FormatVersion::CURRENT.get());
    assert_eq!(answered_format(&finalized), current);
    assert_eq!(stamp(&backend), FormatVersion::CURRENT.get());
    session
        .run("DEFINE INDEX by_doc ON notes FIELDS doc CONTAINS;")
        .unwrap();

    // Again: nothing to raise, and it says what the store holds.
    let again = session.run("ALTER STORE FINALIZE FORMAT;").unwrap();
    assert_eq!(answered_format(&again), current);
    assert_eq!(stamp(&backend), FormatVersion::CURRENT.get());
}

#[test]
fn a_node_whose_older_build_applied_the_finalize_raises_its_stamp_when_it_opens() {
    let (backend, store) = held_at_five();
    Session::new(&store)
        .run("ALTER STORE FINALIZE FORMAT;")
        .unwrap();
    drop(store);
    // What an older build leaves after applying the record: the record, and
    // its own stamp where it was.
    stamped(&backend, 5);
    let reopened = Store::open(Arc::clone(&backend)).unwrap();
    assert_eq!(stamp(&backend), FormatVersion::CURRENT.get());
    assert_eq!(reopened.held_format().unwrap(), FormatVersion::CURRENT);
}

#[test]
fn a_finalize_waits_for_every_replica_to_run_a_build_that_writes_the_format() {
    let (backend, store) = held_at_five();
    Session::new(&store)
        .run(&format!(
            "DEFINE REPLICA peer AT 'p:9001' NODE '{PEER_HEX}' ROLES coordinating \
             REPLICATES STORE;"
        ))
        .unwrap();
    store.hold(Epoch::new(1), Lease::taken(Duration::from_secs(60)));
    let finalize = || Session::new(&store).run("ALTER STORE FINALIZE FORMAT;");

    match finalize() {
        Err(Error::FormatPeerTooOld { name, heard, .. }) => {
            assert_eq!(name, "peer");
            assert!(heard.contains("has not reported"), "{heard}");
        }
        other => panic!("a peer never heard from was finalized over: {other:?}"),
    }
    let needs = FormatVersion::CURRENT.first_written_by().unwrap();
    let older = NodeVersion {
        minor: needs.minor.saturating_sub(1),
        ..needs
    };
    store.follower_greeted(PEER, older);
    assert!(
        matches!(finalize(), Err(Error::FormatPeerTooOld { .. })),
        "a peer on {older} was finalized over"
    );
    assert_eq!(stamp(&backend), 5, "a refused finalize moved the stamp");

    store.follower_greeted(PEER, needs);
    finalize().unwrap();
    assert_eq!(stamp(&backend), FormatVersion::CURRENT.get());
}

/// The record travels; each replica raises its own stamp as it applies it.
#[test]
fn a_follower_raises_its_stamp_when_it_applies_the_finalize() {
    let (_, leader) = held_at_five();
    let (follower_backend, follower) = held_at_five();
    Session::new(&leader)
        .run("ALTER STORE FINALIZE FORMAT;")
        .unwrap();
    for log in leader.logs().unwrap() {
        for (sequence, record) in leader
            .log_records(log, tessari_types::Sequence::ZERO, 4096)
            .unwrap()
        {
            follower
                .apply_record(log.writer, sequence, &record)
                .unwrap();
        }
    }
    assert_eq!(stamp(&follower_backend), FormatVersion::CURRENT.get());
}
