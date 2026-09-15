//! What snapshot isolation guarantees, and what it permits.
//!
//! Half of this file asserts the guarantees. The other half asserts the
//! **anomalies** — write skew and the lost-update-shaped race it looks like —
//! because an anomaly that is only described in prose is one a user discovers
//! in production. Pinning it in a test means it cannot quietly change, in
//! either direction.
//!
//! The concurrency tests run their invariant repeatedly and with real threads.
//! A scheduling-dependent assertion that passed once has proved nothing.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Error, RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

/// How many times each concurrency invariant is re-run.
const CONCURRENCY_RUNS: u32 = 25;
/// Threads competing in each concurrency run.
const WRITERS: u32 = 8;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
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
    let mut txn = store.begin().unwrap();
    txn.put(at(id), payload.to_vec());
    txn.commit().unwrap();
}

fn read(store: &Store, id: &str) -> Option<Vec<u8>> {
    store.begin().unwrap().get(&at(id)).unwrap()
}

// ---------------------------------------------------------------- guarantees

#[test]
fn a_transaction_sees_its_own_writes_before_commit() {
    let store = store();
    let mut txn = store.begin().unwrap();
    txn.put(at("r"), b"pending".to_vec());

    assert_eq!(txn.get(&at("r")).unwrap(), Some(b"pending".to_vec()));
    // ...and nobody else does yet.
    assert_eq!(read(&store, "r"), None);

    txn.commit().unwrap();
    assert_eq!(read(&store, "r"), Some(b"pending".to_vec()));
}

#[test]
fn a_transaction_sees_a_deletion_it_has_not_committed() {
    let store = store();
    write(&store, "r", b"value");

    let mut txn = store.begin().unwrap();
    txn.delete(at("r"));
    assert_eq!(txn.get(&at("r")).unwrap(), None);
    // Other readers still see it until the delete commits.
    assert_eq!(read(&store, "r"), Some(b"value".to_vec()));

    txn.commit().unwrap();
    assert_eq!(read(&store, "r"), None);
}

#[test]
fn a_commit_after_begin_is_invisible_to_the_open_transaction() {
    let store = store();
    write(&store, "r", b"first");

    let reader = store.begin().unwrap();
    write(&store, "r", b"second");

    // The open transaction still reads at its own snapshot.
    assert_eq!(reader.get(&at("r")).unwrap(), Some(b"first".to_vec()));
    // A fresh transaction sees the new value.
    assert_eq!(read(&store, "r"), Some(b"second".to_vec()));
}

#[test]
fn an_older_snapshot_still_sees_a_record_that_was_deleted_later() {
    let store = store();
    write(&store, "r", b"alive");

    let reader = store.begin().unwrap();
    let mut deleter = store.begin().unwrap();
    deleter.delete(at("r"));
    deleter.commit().unwrap();

    assert_eq!(reader.get(&at("r")).unwrap(), Some(b"alive".to_vec()));
    assert_eq!(read(&store, "r"), None);
}

#[test]
fn the_first_committer_wins_and_the_loser_writes_nothing() {
    let store = store();
    write(&store, "r", b"base");

    let mut first = store.begin().unwrap();
    let mut second = store.begin().unwrap();
    first.put(at("r"), b"first".to_vec());
    second.put(at("r"), b"second".to_vec());

    let winner = first.commit().unwrap();
    let error = second.commit().unwrap_err();

    match error {
        Error::Conflict {
            snapshot,
            committed,
            ..
        } => {
            assert_eq!(committed, winner);
            assert!(committed > snapshot);
        }
        other => panic!("expected a conflict, got {other}"),
    }
    assert!(!error.is_retryable(), "a conflict needs a fresh decision");
    assert_eq!(read(&store, "r"), Some(b"first".to_vec()));
}

#[test]
fn a_losing_commit_writes_none_of_its_records_not_even_the_uncontended_ones() {
    let store = store();
    write(&store, "contended", b"base");

    let mut first = store.begin().unwrap();
    let mut second = store.begin().unwrap();
    first.put(at("contended"), b"first".to_vec());
    second.put(at("contended"), b"second".to_vec());
    second.put(at("untouched"), b"collateral".to_vec());

    first.commit().unwrap();
    assert!(second.commit().is_err());

    // The uncontended record from the losing transaction must not exist: a
    // commit is all or nothing.
    assert_eq!(read(&store, "untouched"), None);
}

#[test]
fn an_empty_commit_succeeds_and_advances_nothing() {
    let store = store();
    write(&store, "r", b"value");
    let in_the_range = store
        .committed_tail(store.own_log(crate::FIXTURE_HOME).unwrap())
        .unwrap();

    // An empty commit writes no record, so it names no range — and a position
    // it could answer with has to come from somewhere. It answers the store's
    // own tail, which is the log every node reports while a greeting carries one
    // (Q-622); what "advances nothing" means is that no log moved.
    let txn = store.begin().unwrap();
    assert_eq!(
        txn.commit().unwrap(),
        store
            .committed_tail(store.own_log(tessari_types::Reach::Store).unwrap())
            .unwrap()
    );
    assert_eq!(
        store
            .committed_tail(store.own_log(crate::FIXTURE_HOME).unwrap())
            .unwrap(),
        in_the_range
    );
}

#[test]
fn a_rolled_back_transaction_leaves_no_trace() {
    // There is no "use after close" to test for: `commit` and `rollback` both
    // consume the transaction, so the type system rules it out rather than a
    // runtime flag. What is worth asserting is that a discarded transaction
    // wrote nothing and did not move the committed tail.
    let store = store();
    write(&store, "kept", b"value");
    let before = store
        .committed_tail(store.own_log(crate::FIXTURE_HOME).unwrap())
        .unwrap();

    let mut abandoned = store.begin().unwrap();
    abandoned.put(at("discarded"), b"never".to_vec());
    abandoned.rollback();

    assert_eq!(read(&store, "discarded"), None);
    assert_eq!(read(&store, "kept"), Some(b"value".to_vec()));
    assert_eq!(
        store
            .committed_tail(store.own_log(crate::FIXTURE_HOME).unwrap())
            .unwrap(),
        before
    );
}

// ----------------------------------------------------------- permitted anomalies

#[test]
fn write_skew_happens_and_that_is_the_declared_contract() {
    // The invariant: at least one of `on_call_a` / `on_call_b` stays "yes".
    // Both transactions read both records, each finds the invariant satisfied
    // if it alone steps down, and each writes a DIFFERENT record. Conflict
    // detection is over what was written, so neither sees the other.
    let store = store();
    write(&store, "on_call_a", b"yes");
    write(&store, "on_call_b", b"yes");

    let mut a = store.begin().unwrap();
    let mut b = store.begin().unwrap();

    assert_eq!(a.get(&at("on_call_b")).unwrap(), Some(b"yes".to_vec()));
    assert_eq!(b.get(&at("on_call_a")).unwrap(), Some(b"yes".to_vec()));

    a.put(at("on_call_a"), b"no".to_vec());
    b.put(at("on_call_b"), b"no".to_vec());

    a.commit().unwrap();
    b.commit().expect("write skew is permitted at this level");

    assert_eq!(read(&store, "on_call_a"), Some(b"no".to_vec()));
    assert_eq!(read(&store, "on_call_b"), Some(b"no".to_vec()));
    // The invariant is violated, and no error was raised anywhere. This is what
    // snapshot isolation means, stated as a test rather than as a footnote.
}

#[test]
fn materialising_the_constraint_turns_write_skew_into_a_detected_conflict() {
    // The documented workaround: give the invariant its own record, and make
    // every transaction that depends on it WRITE that record. The skew becomes
    // an ordinary write-write conflict.
    let store = store();
    write(&store, "on_call_a", b"yes");
    write(&store, "on_call_b", b"yes");
    write(&store, "on_call_guard", b"0");

    let mut a = store.begin().unwrap();
    let mut b = store.begin().unwrap();

    a.put(at("on_call_a"), b"no".to_vec());
    a.put(at("on_call_guard"), b"1".to_vec());
    b.put(at("on_call_b"), b"no".to_vec());
    b.put(at("on_call_guard"), b"1".to_vec());

    a.commit().unwrap();
    let error = b.commit().unwrap_err();
    assert!(matches!(error, Error::Conflict { .. }), "{error}");

    assert_eq!(read(&store, "on_call_b"), Some(b"yes".to_vec()));
}

// ------------------------------------------------------------- under real threads

#[test]
fn concurrent_writers_to_one_record_never_lose_an_update() {
    // Each writer reads a counter, increments it, and commits, retrying on
    // conflict with a fresh snapshot — which is the correct response to a
    // conflict, since a conflict means the read was stale.
    //
    // Run repeatedly: a single green pass of a scheduling-dependent assertion
    // proves nothing about the schedules it did not happen to take.
    for run in 0..CONCURRENCY_RUNS {
        let store = store();
        write(&store, "counter", b"0");
        let conflicts = Arc::new(AtomicU32::new(0));

        thread::scope(|scope| {
            for _ in 0..WRITERS {
                let store = store.clone();
                let conflicts = Arc::clone(&conflicts);
                scope.spawn(move || {
                    loop {
                        let mut txn = store.begin().unwrap();
                        let current: u32 =
                            String::from_utf8(txn.get(&at("counter")).unwrap().unwrap())
                                .unwrap()
                                .parse()
                                .unwrap();
                        txn.put(
                            at("counter"),
                            current.saturating_add(1).to_string().into_bytes(),
                        );
                        match txn.commit() {
                            Ok(_) => break,
                            Err(Error::Conflict { .. } | Error::CommitContention { .. }) => {
                                conflicts.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(other) => panic!("unexpected error: {other}"),
                        }
                    }
                });
            }
        });

        let final_value = String::from_utf8(read(&store, "counter").unwrap()).unwrap();
        assert_eq!(
            final_value,
            WRITERS.to_string(),
            "run {run}: an update was lost ({} conflicts observed)",
            conflicts.load(Ordering::Relaxed)
        );
    }
}

#[test]
fn concurrent_writers_to_different_records_all_succeed() {
    // Disjoint writes must not conflict with each other. They do serialise on
    // the committed tail, so this also exercises the retry path that the tail's
    // compare-and-set produces under contention.
    for run in 0..CONCURRENCY_RUNS {
        let store = store();

        thread::scope(|scope| {
            for writer in 0..WRITERS {
                let store = store.clone();
                scope.spawn(move || {
                    let id = format!("record-{writer}");
                    loop {
                        let mut txn = store.begin().unwrap();
                        txn.put(at(&id), writer.to_string().into_bytes());
                        match txn.commit() {
                            Ok(_) => break,
                            Err(Error::CommitContention { .. }) => continue,
                            Err(other) => panic!("unexpected error: {other}"),
                        }
                    }
                });
            }
        });

        for writer in 0..WRITERS {
            let id = format!("record-{writer}");
            assert_eq!(
                read(&store, &id),
                Some(writer.to_string().into_bytes()),
                "run {run}: writer {writer} lost its write"
            );
        }
    }
}

#[test]
fn a_reader_holding_a_snapshot_is_unaffected_by_concurrent_commits() {
    for run in 0..CONCURRENCY_RUNS {
        let store = store();
        write(&store, "r", b"original");
        let reader = store.begin().unwrap();

        thread::scope(|scope| {
            for writer in 0..WRITERS {
                let store = store.clone();
                scope.spawn(move || {
                    loop {
                        let mut txn = store.begin().unwrap();
                        txn.put(at("r"), format!("v{writer}").into_bytes());
                        match txn.commit() {
                            Ok(_) => break,
                            Err(Error::Conflict { .. } | Error::CommitContention { .. }) => {}
                            Err(other) => panic!("unexpected error: {other}"),
                        }
                    }
                });
            }
        });

        assert_eq!(
            reader.get(&at("r")).unwrap(),
            Some(b"original".to_vec()),
            "run {run}: a held snapshot saw a later commit"
        );
    }
}
