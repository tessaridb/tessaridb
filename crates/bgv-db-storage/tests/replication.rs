//! State is a deterministic function of the log — asserted, not described.
//!
//! The decisive test here is `replay_into_an_empty_store_reproduces_it_byte_for_byte`.
//! It compares raw bytes of every keyspace rather than decoded values, because
//! "the same data" and "the same bytes" are different claims and only the second
//! one survives contact with a replica that has to agree with its leader about
//! what it holds.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use bgv_db_encoding::{LogRecord, Mutation, RecordValue};
use bgv_db_kv::{Key, KeyRange, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value};
use bgv_db_storage::{Error, RecordAddress, Store};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

/// How many records the log is read in one go. Larger than any test writes.
const PLENTY: usize = 1024;

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>
}

fn store_on(backend: &Arc<dyn KvBackend>) -> Store {
    Store::open(Arc::clone(backend)).unwrap()
}

fn at(id: &str) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(id),
    )
}

fn write(store: &Store, id: &str, payload: &[u8]) -> Sequence {
    let mut transaction = store.begin().unwrap();
    transaction.put(at(id), payload.to_vec());
    transaction.commit().unwrap()
}

fn delete(store: &Store, id: &str) -> Sequence {
    let mut transaction = store.begin().unwrap();
    transaction.delete(at(id));
    transaction.commit().unwrap()
}

/// Every key and value in one keyspace, as raw bytes.
fn dump(backend: &Arc<dyn KvBackend>, keyspace: Keyspace) -> Vec<(Key, Value)> {
    backend
        .scan(&ScanRequest::new(keyspace, KeyRange::all()))
        .unwrap()
}

#[test]
fn every_commit_leaves_exactly_one_log_record_at_its_own_sequence() {
    let store = store_on(&backend());
    let first = write(&store, "a", b"1");
    let second = write(&store, "b", b"2");
    let third = delete(&store, "a");

    let records = store.log_records(Sequence::ZERO, PLENTY).unwrap();
    let sequences: Vec<Sequence> = records.iter().map(|(sequence, _)| *sequence).collect();
    assert_eq!(sequences, vec![first, second, third]);
}

#[test]
fn the_log_is_gap_free_and_reads_oldest_first() {
    let store = store_on(&backend());
    for n in 0..10 {
        write(&store, &format!("r{n}"), b"v");
    }

    let records = store.log_records(Sequence::ZERO, PLENTY).unwrap();
    assert_eq!(records.len(), 10);
    for (index, (sequence, _)) in records.iter().enumerate() {
        let expected = u64::try_from(index).unwrap().saturating_add(1);
        assert_eq!(sequence.get(), expected, "the log skipped a position");
    }
}

#[test]
fn a_log_read_can_resume_from_a_position() {
    let store = store_on(&backend());
    for n in 0..5 {
        write(&store, &format!("r{n}"), b"v");
    }

    let tail = store.log_records(Sequence::new(4), PLENTY).unwrap();
    let sequences: Vec<u64> = tail.iter().map(|(sequence, _)| sequence.get()).collect();
    assert_eq!(sequences, vec![4, 5]);
}

#[test]
fn an_empty_transaction_never_reaches_the_log() {
    let store = store_on(&backend());
    write(&store, "r", b"v");
    let before = store.log_records(Sequence::ZERO, PLENTY).unwrap().len();

    let empty = store.begin().unwrap();
    empty.commit().unwrap();

    assert_eq!(
        store.log_records(Sequence::ZERO, PLENTY).unwrap().len(),
        before,
        "a commit that changes nothing must not occupy a sequence"
    );
}

#[test]
fn replay_into_an_empty_store_reproduces_it_byte_for_byte() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    write(&source, "alpha", b"one");
    write(&source, "beta", b"two");
    write(&source, "alpha", b"one-again");
    delete(&source, "beta");
    write(&source, "gamma", &[0x00, 0xff, 0x00]);

    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    for (sequence, record) in source.log_records(Sequence::ZERO, PLENTY).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    // The node identity is the one key a replay must **not** reproduce: it is in
    // `META` precisely so that it does not travel (ADR-0018 §1), because a
    // replica holding the source's id is a second process answering to one
    // identity. Excluded here and asserted to differ below — the hole alone
    // would also cover the key vanishing.
    let node_identity = Key::from(vec![0x38]);
    for keyspace in Keyspace::ALL {
        let derived = |backend: &Arc<dyn KvBackend>| {
            dump(backend, *keyspace)
                .into_iter()
                .filter(|(key, _)| *key != node_identity)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            derived(&source_backend),
            derived(&replica_backend),
            "keyspace {keyspace} differs after replay"
        );
    }
    assert_ne!(
        source.node_identity().id,
        replica.node_identity().id,
        "the replica came up holding the source's identity"
    );
    assert_eq!(
        replica.committed_tail().unwrap(),
        source.committed_tail().unwrap()
    );
}

#[test]
fn a_replica_reads_what_the_source_reads_including_the_history_behind_it() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    let first = write(&source, "r", b"first");
    write(&source, "r", b"second");

    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    for (sequence, record) in source.log_records(Sequence::ZERO, PLENTY).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

    assert_eq!(
        replica.begin().unwrap().get(&at("r")).unwrap(),
        Some(b"second".to_vec())
    );
    // The older version survived the replay too, so a snapshot taken on the
    // replica reads the same history the source would have shown.
    let older = replica.begin().unwrap();
    assert_eq!(older.snapshot(), source.committed_tail().unwrap());
    assert!(first < older.snapshot());
}

#[test]
fn applying_the_same_log_twice_changes_nothing_the_second_time() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    write(&source, "a", b"1");
    write(&source, "b", b"2");
    let log = source.log_records(Sequence::ZERO, PLENTY).unwrap();

    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    for (sequence, record) in &log {
        replica.apply_record(*sequence, record).unwrap();
    }
    let after_first_pass: Vec<Vec<(Key, Value)>> = Keyspace::ALL
        .iter()
        .map(|keyspace| dump(&replica_backend, *keyspace))
        .collect();

    // Re-sending a record a replica already holds is an ordinary retry, not an
    // error, and it must not move anything.
    for (sequence, record) in &log {
        replica.apply_record(*sequence, record).unwrap();
    }

    for (keyspace, before) in Keyspace::ALL.iter().zip(after_first_pass) {
        assert_eq!(
            dump(&replica_backend, *keyspace),
            before,
            "re-applying moved keyspace {keyspace}"
        );
    }
}

#[test]
fn a_gap_in_the_log_is_refused_rather_than_skipped() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    write(&source, "a", b"1");
    write(&source, "b", b"2");
    write(&source, "c", b"3");
    let log = source.log_records(Sequence::ZERO, PLENTY).unwrap();

    let replica = store_on(&backend());
    replica.apply_record(log[0].0, &log[0].1).unwrap();

    // Skip the second record.
    let error = replica.apply_record(log[2].0, &log[2].1).unwrap_err();
    match error {
        Error::LogGap { expected, found } => {
            assert_eq!(expected, Sequence::new(2));
            assert_eq!(found, Sequence::new(3));
        }
        other => panic!("expected a gap, got {other}"),
    }
    assert!(!error.is_retryable(), "the same record will still be wrong");
    assert_eq!(replica.committed_tail().unwrap(), Sequence::new(1));
}

#[test]
fn a_replica_that_applied_a_record_can_still_commit_of_its_own_accord() {
    // Not a mode this store will run in for real — a replica does not accept
    // local writes — but it proves the two paths leave the store in the same
    // shape rather than in two shapes that only look alike.
    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    let record = LogRecord::new(vec![Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table: TableId::new(1),
        id: RecordId::from("applied"),
        value: RecordValue::Present(b"from-the-log".to_vec()),
    }]);
    replica.apply_record(Sequence::new(1), &record).unwrap();

    let committed = write(&replica, "local", b"from-a-commit");
    assert_eq!(committed, Sequence::new(2));

    let transaction = replica.begin().unwrap();
    assert_eq!(
        transaction.get(&at("applied")).unwrap(),
        Some(b"from-the-log".to_vec())
    );
    assert_eq!(
        transaction.get(&at("local")).unwrap(),
        Some(b"from-a-commit".to_vec())
    );
    assert_eq!(
        replica.log_records(Sequence::ZERO, PLENTY).unwrap().len(),
        2,
        "both paths left one log record each"
    );
}
