//! State is a deterministic function of the log — asserted, not described.
//!
//! The decisive test here is `replay_into_an_empty_store_reproduces_it_byte_for_byte`.
//! It compares raw bytes of every keyspace rather than decoded values, because
//! "the same data" and "the same bytes" are different claims and only the second
//! one survives contact with a replica that has to agree with its leader about
//! what it holds.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_encoding::{LogRecord, Mutation, RecordValue, encode_payload};
use tessari_kv::{Key, KeyRange, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value};
use tessari_storage::{Catalog, EDGE_IN, EDGE_OUT, Error, RecordAddress, Store, TableShape};
use tessari_types::{
    DatabaseId, Epoch, NamespaceId, RecordId, RecordRef, Sequence, TableId, Value as FieldValue,
};

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
        source.node_identity().unwrap().id,
        replica.node_identity().unwrap().id,
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
fn two_leaderships_writing_one_sequence_are_refused_rather_than_silently_dropped() {
    // S1.1. The failure this test exists for is not a wrong answer — it is the
    // absence of one. Before the epoch, a node fed a record at a sequence it had
    // already written took the already-applied branch and returned `Ok`, keeping
    // its own divergent data with no error, no gap and no signal. Two nodes then
    // answered differently while both reported healthy.
    let held = store_on(&backend());
    let offered = LogRecord::at(
        Epoch::new(2),
        vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("contested"),
            value: RecordValue::Present(b"from-the-new-leader".to_vec()),
        }],
    );
    held.apply_record(
        Sequence::new(1),
        &LogRecord::at(
            Epoch::new(1),
            vec![Mutation {
                namespace: NamespaceId::new(1),
                database: DatabaseId::new(1),
                table: TableId::new(1),
                id: RecordId::from("contested"),
                value: RecordValue::Present(b"from-the-old-leader".to_vec()),
            }],
        ),
    )
    .unwrap();

    let error = held.apply_record(Sequence::new(1), &offered).unwrap_err();
    match error {
        Error::LogDivergence {
            sequence,
            held: was,
            offered: now,
        } => {
            assert_eq!(sequence, Sequence::new(1));
            assert_eq!(was, Epoch::new(1));
            assert_eq!(now, Epoch::new(2));
        }
        other => panic!("expected a divergence, got {other}"),
    }
    assert!(
        !error.is_retryable(),
        "re-sending the same record cannot resolve a divergence — the node re-bootstraps"
    );
    assert_eq!(
        held.health().unwrap().log_divergences,
        1,
        "S1.3 — a divergence nobody counts is a divergence nobody notices"
    );
}

#[test]
fn re_sending_a_record_the_store_already_holds_stays_a_free_no_op() {
    // The other half, and the one that must not regress: an ordinary retry is
    // not a divergence, and refusing it would turn re-delivery into an incident.
    let held = store_on(&backend());
    let record = LogRecord::at(
        Epoch::new(1),
        vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("repeated"),
            value: RecordValue::Present(b"v".to_vec()),
        }],
    );
    held.apply_record(Sequence::new(1), &record).unwrap();
    held.apply_record(Sequence::new(1), &record).unwrap();
    held.apply_record(Sequence::new(1), &record).unwrap();
    assert_eq!(held.committed_tail().unwrap(), Sequence::new(1));
    assert_eq!(held.health().unwrap().log_divergences, 0);
}

#[test]
fn replaying_an_older_record_from_an_older_leadership_is_not_a_divergence() {
    // The trap a current-epoch counter would fall into: a follower catching up
    // from the start of the log legitimately offers records written under
    // leaderships that have since ended. Compared against the epoch the store
    // holds *at that sequence*, they match; compared against the store's latest
    // epoch, every one of them would be a false divergence.
    let held = store_on(&backend());
    let first = LogRecord::at(
        Epoch::new(1),
        vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("old"),
            value: RecordValue::Present(b"a".to_vec()),
        }],
    );
    let second = LogRecord::at(
        Epoch::new(4),
        vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("new"),
            value: RecordValue::Present(b"b".to_vec()),
        }],
    );
    held.apply_record(Sequence::new(1), &first).unwrap();
    held.apply_record(Sequence::new(2), &second).unwrap();

    held.apply_record(Sequence::new(1), &first).unwrap();
    assert_eq!(held.health().unwrap().log_divergences, 0);
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

#[test]
fn a_replay_derives_the_adjacency_the_commit_derived() {
    // Q-452. `adjacency.rs` opens by naming this failure as the reason it
    // derives from the mutation rather than letting a caller write the entries:
    // "a replica reaches its state by replaying that record ... the symptom
    // would be a follower whose walks find nothing while the leader answers
    // correctly, with nothing anywhere in an error state." The replay path
    // reproduced that symptom by the other route, because it called two of the
    // three `maintain` functions.
    //
    // The keyspace comparison that catches it has been here since the file was
    // written and passed throughout, because the fixture above writes three
    // plain records and no edge. So this case is the fixture and not the
    // assertion: its own catalog, because the one above writes through a raw
    // address with no tables in it at all.
    let source_backend = backend();
    let source = store_on(&source_backend);

    let mut transaction = source.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "social").unwrap();
    let people = catalog
        .create_table(namespace.id, database.id, "person", TableShape::default())
        .unwrap();
    let edges = catalog
        .create_table(namespace.id, database.id, "knows", TableShape::default())
        .unwrap();
    let graph = catalog
        .create_graph(namespace.id, database.id, "social")
        .unwrap();
    catalog
        .create_edge_kind(&graph, "knows", people.id, people.id, edges.id)
        .unwrap();
    transaction.commit().unwrap();

    let mut fields = BTreeMap::new();
    fields.insert(
        EDGE_OUT.to_owned(),
        FieldValue::Record(RecordRef::new(people.id, RecordId::from("ana"))),
    );
    fields.insert(
        EDGE_IN.to_owned(),
        FieldValue::Record(RecordRef::new(people.id, RecordId::from("ben"))),
    );
    let before = dump(&source_backend, Keyspace::INDEX).len();
    let mut transaction = source.begin().unwrap();
    transaction.put(
        RecordAddress::new(namespace.id, database.id, edges.id, RecordId::from("e1")),
        encode_payload(&FieldValue::Object(fields)).into_bytes(),
    );
    transaction.commit().unwrap();

    // The leader derived an entry at each end of the edge. Checked rather than
    // assumed: if the commit path derived nothing, the comparison below would
    // pass by finding two stores that are equally empty, which is the way this
    // case could silently stop testing anything.
    assert_eq!(
        dump(&source_backend, Keyspace::INDEX).len(),
        before + 2,
        "the commit path derived no adjacency, so this case cannot test the replay"
    );

    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    for (sequence, record) in source.log_records(Sequence::ZERO, PLENTY).unwrap() {
        replica.apply_record(sequence, &record).unwrap();
    }

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
            "keyspace {keyspace} differs after replaying a log that carries an edge"
        );
    }
}
