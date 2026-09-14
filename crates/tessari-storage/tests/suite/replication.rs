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

use tessari_encoding::{LogRecord, Mutation, RecordValue, StampedValue, encode_payload};
use tessari_kv::{Key, KeyRange, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value};
use tessari_storage::{Catalog, EDGE_IN, EDGE_OUT, Error, Reach, RecordAddress, Store, TableShape};
use tessari_types::{
    DatabaseId, Epoch, NamespaceId, RecordId, RecordRef, ReplicationClass, Sequence, TableId,
    Value as FieldValue,
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

/// One put, addressed like [`at`].
fn mutation(id: &str, value: &[u8]) -> Mutation {
    Mutation {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(1),
        table: TableId::new(1),
        id: RecordId::from(id),
        value: StampedValue::new(RecordValue::Present(value.to_vec())),
    }
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

    let records = store
        .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
        .unwrap();
    let sequences: Vec<Sequence> = records.iter().map(|(sequence, _)| *sequence).collect();
    assert_eq!(sequences, vec![first, second, third]);
}

#[test]
fn the_log_is_gap_free_and_reads_oldest_first() {
    let store = store_on(&backend());
    for n in 0..10 {
        write(&store, &format!("r{n}"), b"v");
    }

    let records = store
        .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
        .unwrap();
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

    let tail = store
        .log_records(crate::FIXTURE_HOME, Sequence::new(4), PLENTY)
        .unwrap();
    let sequences: Vec<u64> = tail.iter().map(|(sequence, _)| sequence.get()).collect();
    assert_eq!(sequences, vec![4, 5]);
}

#[test]
fn an_empty_transaction_never_reaches_the_log() {
    let store = store_on(&backend());
    write(&store, "r", b"v");
    let before = store
        .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
        .unwrap()
        .len();

    let empty = store.begin().unwrap();
    empty.commit().unwrap();

    assert_eq!(
        store
            .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
            .unwrap()
            .len(),
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
    crate::replay(&source, &replica);

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
        replica.committed_tail(crate::FIXTURE_HOME).unwrap(),
        source.committed_tail(crate::FIXTURE_HOME).unwrap()
    );
}

#[test]
fn a_replica_reads_what_the_source_reads_including_the_history_behind_it() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    write(&source, "r", b"first");
    let source_held_first = source.begin().unwrap().snapshot();
    write(&source, "r", b"second");

    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    let mut replica_held_first = None;
    for (index, (sequence, record)) in source
        .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
        .unwrap()
        .into_iter()
        .enumerate()
    {
        replica.apply_record(sequence, &record).unwrap();
        if index == 0 {
            replica_held_first = Some(replica.begin().unwrap().snapshot());
        }
    }
    let replica_held_first = replica_held_first.unwrap();

    assert_eq!(
        replica.begin().unwrap().get(&at("r")).unwrap(),
        Some(b"second".to_vec())
    );
    // The older version survived the replay too — asserted as the STATE each
    // store reads at its OWN earlier snapshot, and deliberately not as the two
    // of them agreeing on a number. A record version is a fact about one
    // store's visible history and a log position is the fact the two share, so
    // an assertion that a replica's snapshot equals the source's committed tail
    // is an assertion about the wrong one (Q-614). It held while a single flat
    // log made every version equal its position, which is exactly why it was
    // worth writing down before that stopped being true.
    assert_eq!(
        replica
            .begin_at(replica_held_first)
            .unwrap()
            .get(&at("r"))
            .unwrap(),
        source
            .begin_at(source_held_first)
            .unwrap()
            .get(&at("r"))
            .unwrap(),
        "the replica's own history answers what the source's answers"
    );
    assert_eq!(
        replica
            .begin_at(replica_held_first)
            .unwrap()
            .get(&at("r"))
            .unwrap(),
        Some(b"first".to_vec())
    );
    assert!(replica_held_first < replica.begin().unwrap().snapshot());
}

#[test]
fn applying_the_same_log_twice_changes_nothing_the_second_time() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    write(&source, "a", b"1");
    write(&source, "b", b"2");
    let log = source
        .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
        .unwrap();

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
    let log = source
        .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
        .unwrap();

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
    assert_eq!(
        replica.committed_tail(crate::FIXTURE_HOME).unwrap(),
        Sequence::new(1)
    );
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
            value: StampedValue::new(RecordValue::Present(b"from-the-new-leader".to_vec())),
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
                value: StampedValue::new(RecordValue::Present(b"from-the-old-leader".to_vec())),
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

/// A store with `prod`/`orders` declared, and the namespace given `class`.
///
/// Returns the store, the home a record in that database files at, and the
/// mutation address to write there — derived rather than assumed, because
/// `create_namespace` allocates the id and a test that hard-codes `1` passes
/// for the wrong reason the day allocation changes.
fn declared(class: Option<ReplicationClass>) -> (Store, Reach, NamespaceId, DatabaseId) {
    let store = store_on(&backend());
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    if let Some(class) = class {
        catalog.set_replication_class(namespace.id, class).unwrap();
    }
    transaction.commit().unwrap();
    (
        store,
        Reach::Database(namespace.id, database.id),
        namespace.id,
        database.id,
    )
}

/// One put into `declared`'s database, under `epoch`.
fn contested(
    namespace: NamespaceId,
    database: DatabaseId,
    epoch: Epoch,
    value: &[u8],
) -> LogRecord {
    LogRecord::at(
        epoch,
        vec![Mutation {
            namespace,
            database,
            table: TableId::new(1),
            id: RecordId::from("contested"),
            value: StampedValue::new(RecordValue::Present(value.to_vec())),
        }],
    )
}

#[test]
fn two_leaderships_on_a_declared_range_are_both_admitted() {
    // S2.1, the half that is new. The same offer the test above refuses, made
    // against a namespace that declared `MULTI MASTER` — and the declaration is
    // the only difference between the two, which is what makes this a scoping
    // of the fence rather than a hole in it.
    let (store, home, namespace, database) = declared(Some(ReplicationClass::MultiMaster));
    let first = Sequence::new(store.committed_tail(home).unwrap().get().saturating_add(1));

    store
        .apply_record(
            first,
            &contested(namespace, database, Epoch::new(1), b"from-one-master"),
        )
        .unwrap();
    store
        .apply_record(
            first,
            &contested(namespace, database, Epoch::new(2), b"from-the-other"),
        )
        .expect("a declared range admits a second leadership");

    assert_eq!(
        store.health().unwrap().log_divergences,
        0,
        "two masters on a declared range are not a divergence, so nothing counts one"
    );
}

#[test]
fn a_range_that_declared_single_leader_refuses_exactly_as_silence_does() {
    // The other side of the declaration, and the reason the class is a stated
    // value rather than a boolean that is only ever set: an operator who
    // answered the question gets the engine's existing behaviour, not a
    // different one, and `INFO FOR` can still tell the two apart.
    for class in [None, Some(ReplicationClass::SingleLeader)] {
        let (store, home, namespace, database) = declared(class);
        let first = Sequence::new(store.committed_tail(home).unwrap().get().saturating_add(1));

        store
            .apply_record(
                first,
                &contested(namespace, database, Epoch::new(1), b"from-the-old-leader"),
            )
            .unwrap();
        let error = store
            .apply_record(
                first,
                &contested(namespace, database, Epoch::new(2), b"from-the-new-leader"),
            )
            .unwrap_err();

        match error {
            Error::LogDivergence { sequence, .. } => assert_eq!(sequence, first),
            other => panic!("expected a divergence for {class:?}, got {other}"),
        }
        assert_eq!(
            store.health().unwrap().log_divergences,
            1,
            "an undeclared and an explicitly single-leader range answer alike"
        );
    }
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
            value: StampedValue::new(RecordValue::Present(b"v".to_vec())),
        }],
    );
    held.apply_record(Sequence::new(1), &record).unwrap();
    held.apply_record(Sequence::new(1), &record).unwrap();
    held.apply_record(Sequence::new(1), &record).unwrap();
    assert_eq!(
        held.committed_tail(crate::FIXTURE_HOME).unwrap(),
        Sequence::new(1)
    );
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
            value: StampedValue::new(RecordValue::Present(b"a".to_vec())),
        }],
    );
    let second = LogRecord::at(
        Epoch::new(4),
        vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("new"),
            value: StampedValue::new(RecordValue::Present(b"b".to_vec())),
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
        value: StampedValue::new(RecordValue::Present(b"from-the-log".to_vec())),
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
        replica
            .log_records(crate::FIXTURE_HOME, Sequence::ZERO, PLENTY)
            .unwrap()
            .len(),
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
    crate::replay(&source, &replica);

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

/// G024 **S3.1**: a follower bootstraps by replaying the log from origin **with
/// the source never stopped**, and ends byte-identical to it.
///
/// The test above this one replays a *quiesced* source, which is the easy half
/// and was already true. The words that carry S3.1 are *never stopped*: a
/// follower reading `log_records(its own tail, n)` is chasing a tail that is
/// moving away from it, and whether that converges — and whether anything
/// refuses on the way — is a question about the engine rather than about the
/// caller's loop.
///
/// Three things are asserted and the second is the one that would be easy to
/// leave out. It **converges** while the leader is still writing. **Every**
/// `apply_record` returns `Ok` — no gap, no divergence, no torn read of a
/// record being committed while the scan ran, which is the case worth looking
/// at because `log_records` scans the backend directly rather than through a
/// transaction snapshot. And the keyspaces match **byte for byte** once both
/// are quiet, because "the same data" and "the same bytes" are different claims.
#[test]
fn a_follower_catches_a_leader_that_is_still_writing() {
    const WRITES: usize = 400;
    /// Small enough that the follower cannot swallow the log in one pass, which
    /// is what makes this a chase rather than the quiesced replay above.
    const BATCH: usize = 16;

    let leader_backend = backend();
    let leader = Arc::new(store_on(&leader_backend));
    let writing = Arc::clone(&leader);

    let writer = std::thread::spawn(move || {
        for n in 0..WRITES {
            write(
                &writing,
                &format!("record-{n:04}"),
                format!("value-{n}").as_bytes(),
            );
        }
    });

    let replica_backend = backend();
    let replica = store_on(&replica_backend);

    // The follower knows only its own tail — it never asks the leader where to
    // start, which is what makes this resumable rather than a one-shot copy.
    let mut applied = 0_usize;
    let mut passes = 0_usize;
    let mut overlapped = 0_usize;
    loop {
        let from = Sequence::new(
            replica
                .committed_tail(crate::FIXTURE_HOME)
                .unwrap()
                .get()
                .saturating_add(1),
        );
        let batch = leader
            .log_records(crate::FIXTURE_HOME, from, BATCH)
            .unwrap();
        if batch.is_empty() {
            if writer.is_finished()
                && replica.committed_tail(crate::FIXTURE_HOME).unwrap()
                    == leader.committed_tail(crate::FIXTURE_HOME).unwrap()
            {
                break;
            }
            std::thread::yield_now();
            continue;
        }
        passes = passes.saturating_add(1);
        if !writer.is_finished() {
            overlapped = overlapped.saturating_add(1);
        }
        for (sequence, record) in batch {
            // Asserted rather than unwrapped away: a refusal here is the whole
            // question, and `unwrap` would report it as a panic in a helper.
            assert!(
                replica.apply_record(sequence, &record).is_ok(),
                "the follower refused record {sequence} mid-bootstrap"
            );
            applied = applied.saturating_add(1);
        }
    }
    writer.join().unwrap();

    // The leader may have written more after the follower's last read, so one
    // final pass settles it. That this pass exists is the honest shape of a
    // bootstrap against a live source and not a weakening of the claim: the
    // loop above ran while writes were in flight, which is what `never stopped`
    // asks for.
    for (sequence, record) in leader
        .log_records(
            crate::FIXTURE_HOME,
            Sequence::new(
                replica
                    .committed_tail(crate::FIXTURE_HOME)
                    .unwrap()
                    .get()
                    .saturating_add(1),
            ),
            PLENTY,
        )
        .unwrap()
    {
        replica.apply_record(sequence, &record).unwrap();
    }

    assert!(passes > 1, "the follower swallowed the log in one pass");
    // The assertion that makes this test about a live source rather than a
    // quiesced one wearing a thread: at least one pass ran while the writer was
    // still going. Measured at 90-278 of them on this machine; the bar is one,
    // because a loaded machine may schedule the writer to completion early and
    // a tighter bound would make this flaky rather than strict.
    assert!(
        overlapped > 0,
        "every pass ran after the writer finished — the source was effectively stopped"
    );
    assert_eq!(
        u64::try_from(applied).unwrap(),
        leader.committed_tail(crate::FIXTURE_HOME).unwrap().get(),
        "the follower applied a different number of records than the leader wrote"
    );
    assert_eq!(
        replica.committed_tail(crate::FIXTURE_HOME).unwrap(),
        leader.committed_tail(crate::FIXTURE_HOME).unwrap(),
        "the follower did not converge on the leader's tail"
    );
    assert_eq!(
        replica.health().unwrap().log_divergences,
        0,
        "a bootstrap against a live source raised a divergence"
    );

    let node_identity = Key::from(vec![0x38]);
    for keyspace in Keyspace::ALL {
        let derived = |backend: &Arc<dyn KvBackend>| {
            dump(backend, *keyspace)
                .into_iter()
                .filter(|(key, _)| *key != node_identity)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            derived(&leader_backend),
            derived(&replica_backend),
            "keyspace {keyspace} differs after a bootstrap against a live source"
        );
    }
}

/// G024 **S1.2**: a follower refuses the **first** record offered after its
/// history parted from the sender's, rather than discovering it later.
///
/// W210 closed the overlapping case — a record offered at a position the store
/// already holds under a different epoch is refused. This is the case it cannot
/// see. The sender's log diverged from the follower's at sequence 4; it now
/// offers sequence 6, which is `tail + 1` for the follower, so there is nothing
/// at that position to compare and the old code **appends it**. The follower
/// then holds 1-5 from one history and 6 from another with no error anywhere.
///
/// The fix is Raft's `AppendEntries` consistency check: the sender states the
/// epoch of the record **before** the one it is offering, and the receiver
/// compares that against what it actually holds there. The refusal names
/// sequence **5** — where the histories part — and not 6, which is merely where
/// the check ran.
#[test]
fn a_record_whose_predecessor_the_follower_never_wrote_is_refused_at_once() {
    let follower_backend = backend();
    let follower = store_on(&follower_backend);
    for n in 1..=5_u64 {
        let record = LogRecord::at(Epoch::new(1), vec![mutation(&format!("record-{n}"), b"v")]);
        follower.apply_record(Sequence::new(n), &record).unwrap();
    }
    assert_eq!(
        follower.committed_tail(crate::FIXTURE_HOME).unwrap(),
        Sequence::new(5)
    );

    // The sender's sixth record. Its own record at 5 was written under epoch 2,
    // because its history parted from this follower's at sequence 4.
    let offered = LogRecord::at(
        Epoch::new(2),
        vec![mutation("record-6", b"from-the-other-history")],
    );

    let error = follower
        .apply_from_stream(
            crate::FIXTURE_HOME,
            Sequence::new(6),
            Epoch::new(2),
            &offered,
        )
        .unwrap_err();
    match error {
        Error::LogDivergence {
            sequence,
            held,
            offered,
        } => {
            assert_eq!(
                sequence,
                Sequence::new(5),
                "the refusal named where the check ran, not where the histories parted"
            );
            assert_eq!(held, Epoch::new(1));
            assert_eq!(offered, Epoch::new(2));
        }
        other => panic!("expected a divergence, got {other}"),
    }

    assert_eq!(
        follower.committed_tail(crate::FIXTURE_HOME).unwrap(),
        Sequence::new(5),
        "the refused record was applied anyway"
    );
    assert_eq!(follower.health().unwrap().log_divergences, 1);
}

#[test]
fn a_stream_whose_predecessor_matches_is_applied_like_any_other_record() {
    let store_backend = backend();
    let store = store_on(&store_backend);
    for n in 1..=3_u64 {
        let record = LogRecord::at(Epoch::new(7), vec![mutation(&format!("record-{n}"), b"v")]);
        store.apply_record(Sequence::new(n), &record).unwrap();
    }

    let next = LogRecord::at(Epoch::new(7), vec![mutation("record-4", b"v")]);
    store
        .apply_from_stream(crate::FIXTURE_HOME, Sequence::new(4), Epoch::new(7), &next)
        .unwrap();
    assert_eq!(
        store.committed_tail(crate::FIXTURE_HOME).unwrap(),
        Sequence::new(4)
    );
    assert_eq!(store.health().unwrap().log_divergences, 0);
}

/// The first record of a fresh log has no predecessor, and a store that has
/// elected nobody holds `Epoch::ZERO` — so the sender claiming zero is correct
/// and needs no special case at the call site. A sender claiming anything else
/// is telling this store about a history it does not have.
#[test]
fn the_first_record_of_a_log_claims_the_epoch_of_a_store_that_elected_nobody() {
    let empty_backend = backend();
    let empty = store_on(&empty_backend);
    let first = LogRecord::at(Epoch::new(3), vec![mutation("record-1", b"v")]);
    empty
        .apply_from_stream(crate::FIXTURE_HOME, Sequence::new(1), Epoch::ZERO, &first)
        .unwrap();
    assert_eq!(
        empty.committed_tail(crate::FIXTURE_HOME).unwrap(),
        Sequence::new(1)
    );

    let other_backend = backend();
    let other = store_on(&other_backend);
    let error = other
        .apply_from_stream(crate::FIXTURE_HOME, Sequence::new(1), Epoch::new(9), &first)
        .unwrap_err();
    assert!(matches!(error, Error::LogDivergence { .. }), "{error}");
}

// ---------------------------------------------------------------------------
// G025 S1.2 — who leads this range, answered from what the log left behind.
//
// The criterion: *a node answers who leads this range from a locally
// materialized view built by log application, with no network call*, validated
// by answering with the peer link down.
//
// There is no link in this module at all — no socket is opened, no greeting is
// exchanged, no peer exists. That is a stronger form of *the link is down* than
// cutting a live one, because a test that cut a link could still be answered by
// something cached from when it was up.
// ---------------------------------------------------------------------------

/// A node id that is not the zero any store would hold by accident.
const LEADER: [u8; tessari_storage::NODE_ID_LEN] = [7; tessari_storage::NODE_ID_LEN];

/// Record a leadership on `store`, exactly as the campaign thread does.
fn lead(store: &Store, range: Reach, epoch: u64) -> Sequence {
    lead_as(store, range, LEADER, epoch)
}

/// Record a leadership held by **this** store, which is the only one it can
/// commit for itself.
///
/// The distinction became load-bearing with ADR-0070: the commit gate resolves
/// the range a write addresses against the leadership catalog, and a row naming
/// somebody else as the leader of the whole store covers the system tenancy too
/// — so a store that has committed *node seven leads everything* may not then
/// commit a second row, and is right not to. In production another node's
/// leadership never arrives by a local commit at all; it arrives by applying the
/// log record that created it, which is what
/// `a_replica_answers_who_leads_from_the_log_it_applied_and_from_nothing_else`
/// does. A test that needs two rows in one catalog is a test about this node.
fn lead_here(store: &Store, range: Reach, epoch: u64) -> Sequence {
    let me = store.node_identity().unwrap().id;
    lead_as(store, range, me, epoch)
}

fn lead_as(
    store: &Store,
    range: Reach,
    node: [u8; tessari_storage::NODE_ID_LEN],
    epoch: u64,
) -> Sequence {
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .record_leadership(range, node, Epoch::new(epoch))
        .unwrap();
    transaction.commit().unwrap()
}

/// What `store` says leads `range`.
fn leads(store: &Store, range: Reach) -> Option<tessari_storage::LeadershipDefinition> {
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction).leader_of(range).unwrap()
}

#[test]
fn a_replica_answers_who_leads_from_the_log_it_applied_and_from_nothing_else() {
    let source_backend = backend();
    let source = store_on(&source_backend);
    lead(&source, Reach::Store, 5);

    // The replica is a separate store on a separate backend. It never speaks to
    // the source: the only thing that crosses is the log.
    let replica_backend = backend();
    let replica = store_on(&replica_backend);
    assert_eq!(
        leads(&replica, Reach::Store),
        None,
        "a store that has applied nothing must not claim to know who leads"
    );

    crate::replay(&source, &replica);

    let answered = leads(&replica, Reach::Store).expect("the log carried the leadership");
    assert_eq!(answered.node, LEADER);
    assert_eq!(
        answered.epoch,
        Epoch::new(5),
        "the answer carries the epoch it was decided under, or a caller holding \
         a newer one cannot tell that this describes a superseded arrangement"
    );
    assert_eq!(answered.range, Reach::Store);
}

#[test]
fn a_new_leadership_replaces_the_previous_answer_rather_than_joining_it() {
    let leader_backend = backend();
    let store = store_on(&leader_backend);
    // This node's own, both of them: a store that has committed somebody else's
    // store-wide leadership may not commit anything afterwards (ADR-0070), and
    // the subject here is which row answers rather than whose name is on it.
    lead_here(&store, Reach::Store, 5);
    lead_here(&store, Reach::Store, 6);

    let held = leads(&store, Reach::Store).unwrap();
    assert_eq!(held.epoch, Epoch::new(6));
    // One row per range, not a history: two answers to *who leads the store*
    // would be two answers to one question, and nothing here chooses between
    // them.
    let mut transaction = store.begin().unwrap();
    assert_eq!(
        Catalog::new(&mut transaction).leaderships().unwrap().len(),
        1
    );
}

#[test]
fn a_range_with_no_leadership_of_its_own_is_answered_by_the_one_above_it() {
    let leader_backend = backend();
    let store = store_on(&leader_backend);
    lead(&store, Reach::Store, 5);

    // Today this is the only case that ever runs: one lease over the whole
    // store, and every range inside it asking the same question.
    let inside = Reach::Database(NamespaceId::new(3), DatabaseId::new(7));
    let answered = leads(&store, inside).expect("the store's leadership covers every range in it");
    assert_eq!(answered.range, Reach::Store);
    assert_eq!(answered.epoch, Epoch::new(5));
}

#[test]
fn the_most_specific_leadership_covering_a_range_is_the_one_that_answers() {
    let leader_backend = backend();
    let store = store_on(&leader_backend);
    let namespace = NamespaceId::new(3);
    let database = DatabaseId::new(7);
    // This node's own, all three: the subject is which of several covering rows
    // answers, and a store that has committed somebody else's store-wide
    // leadership may not commit the next two (ADR-0070).
    lead_here(&store, Reach::Store, 5);
    lead_here(&store, Reach::Namespace(namespace), 6);
    lead_here(&store, Reach::Database(namespace, database), 7);

    // S6.2 splits the epoch by range, and this is the rule that has to be right
    // before it does: a node asked about a database whose leadership is its own
    // must not be answered with the store's.
    assert_eq!(
        leads(&store, Reach::Database(namespace, database))
            .unwrap()
            .range,
        Reach::Database(namespace, database)
    );
    assert_eq!(
        leads(&store, Reach::Namespace(namespace)).unwrap().range,
        Reach::Namespace(namespace)
    );
    assert_eq!(leads(&store, Reach::Store).unwrap().range, Reach::Store);
    // A leadership over one namespace says nothing about another's.
    assert_eq!(
        leads(&store, Reach::Namespace(NamespaceId::new(9)))
            .unwrap()
            .range,
        Reach::Store
    );
}
