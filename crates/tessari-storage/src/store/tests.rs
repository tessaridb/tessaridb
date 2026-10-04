// Test assertions are exactly where a panic is the correct outcome.
#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_encoding::LogRecord;
use tessari_kv::MemoryBackend;

use super::*;
use crate::error::Error;

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new())
}

#[test]
fn a_fresh_store_writes_its_format_and_starts_at_sequence_zero() {
    let store = Store::open(backend()).unwrap();
    assert_eq!(
        store
            .committed_tail(store.own_log(Reach::Store).unwrap())
            .unwrap(),
        Sequence::ZERO
    );
    assert!(store.logs().unwrap().is_empty(), "nothing written yet");
    assert_eq!(
        read_format_version(Arc::clone(store.backend())).unwrap(),
        Some(FormatVersion::CURRENT)
    );
}

#[test]
fn a_fresh_store_starts_its_version_counter_at_zero_as_well() {
    let store = Store::open(backend()).unwrap();
    assert_eq!(store.committed_version().unwrap(), Sequence::ZERO);
}

#[test]
fn a_store_opened_without_a_version_counter_resumes_it_from_the_applied_position() {
    let backend = backend();
    let store = Store::open(Arc::clone(&backend)).unwrap();
    // The shape a store written before the version was separated from the
    // log position has on disk: a position, and no counter beside it. Its
    // records were stamped at that position, so the position *is* the
    // version they hold.
    backend
        .apply(
            WriteBatch::new()
                .put(
                    AppliedPositionKey::keyspace(),
                    AppliedPositionKey::new(LogId::unattributed(Reach::Store)).encode(),
                    Sequence::new(7).encode(),
                )
                .delete(VersionPositionKey::keyspace(), VersionPositionKey.encode()),
        )
        .unwrap();
    drop(store);

    let reopened = Store::open(backend).unwrap();
    assert_eq!(
        reopened.committed_version().unwrap(),
        Sequence::new(7),
        "a counter restarted at zero would reissue versions records already hold"
    );
}

#[test]
fn reopening_a_store_does_not_rewrite_its_metadata() {
    let shared = backend();
    let first = Store::open(Arc::clone(&shared)).unwrap();
    drop(first);
    let second = Store::open(shared).unwrap();
    assert_eq!(
        read_format_version(Arc::clone(second.backend())).unwrap(),
        Some(FormatVersion::CURRENT)
    );
}

#[test]
fn a_store_written_before_the_epoch_opens_and_reads_as_the_first_leadership() {
    // Version 2 must not orphan the stores version 1 already wrote. What the
    // move to version 3 changed is the second half of the old assertion:
    // an older store IS rewritten now, because its log keys no longer decode
    // at all and leaving them would be leaving the store unreadable rather
    // than leaving it alone.
    let shared = backend();
    Store::open(Arc::clone(&shared)).unwrap();
    shared
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(1).encode(),
        ))
        .unwrap();

    let store = Store::open(Arc::clone(&shared)).unwrap();
    assert_eq!(
        read_format_version(Arc::clone(store.backend())).unwrap(),
        Some(FormatVersion::CURRENT),
        "an older log is given its home at open, and the version says so"
    );

    store
        .apply_record(
            Writer::UNATTRIBUTED,
            Sequence::new(1),
            &LogRecord::new(Vec::new()),
        )
        .unwrap();
    let (_, record) = store
        .log_records(LogId::unattributed(Reach::Store), Sequence::new(1), 1)
        .unwrap()
        .pop()
        .expect("the record just applied");
    assert_eq!(
        record.epoch(),
        tessari_types::Epoch::ZERO,
        "a build that elects nobody writes the first and only leadership"
    );
}

#[test]
fn a_log_written_before_it_had_writers_is_rewritten_and_attributed_to_nobody() {
    // The second migration, end to end and against real bytes: a store
    // standing in the shape version 4 left — a homed log key and one
    // position per home, neither naming a writer — is opened, and every
    // record it held is readable afterwards at the position it held, in the
    // log nobody is recorded as having written.
    let shared = backend();
    Store::open(Arc::clone(&shared)).unwrap();
    let home = Reach::Database(
        tessari_types::NamespaceId::new(1),
        tessari_types::DatabaseId::new(2),
    );
    let mut batch = WriteBatch::new().put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::HOMED_LOG.encode(),
    );
    for sequence in 1_u64..=3 {
        // The old shape, written out here rather than built by a helper:
        // this test is the only thing left that knows it, which is the same
        // reason its predecessor above spells its own out.
        let mut key = vec![KeyKind::LogEntry.tag()];
        key.extend_from_slice(&unqualified_home(home));
        key.extend_from_slice(&sequence.to_be_bytes());
        batch = batch.put(
            LogKey::keyspace(),
            Key::from(key),
            LogRecord::new(Vec::new()).encode(),
        );
    }
    let mut position = vec![KeyKind::AppliedPosition.tag()];
    position.extend_from_slice(&unqualified_home(home));
    batch = batch.put(
        AppliedPositionKey::keyspace(),
        Key::from(position),
        Sequence::new(3).encode(),
    );
    shared.apply(batch).unwrap();

    let store = Store::open(Arc::clone(&shared)).unwrap();
    let migrated = LogId::unattributed(home);
    assert_eq!(
        store.logs().unwrap(),
        vec![migrated],
        "the log kept its home and was attributed to nobody, because \
             nothing recorded who wrote it"
    );
    assert_ne!(
        migrated,
        store.own_log(home).unwrap(),
        "and NOT to the node that happened to open the file — a follower's \
             log holds the records its leader wrote"
    );
    assert_eq!(store.committed_tail(migrated).unwrap(), Sequence::new(3));
    let positions: Vec<Sequence> = store
        .log_records(migrated, Sequence::new(1), 16)
        .unwrap()
        .into_iter()
        .map(|(sequence, _)| sequence)
        .collect();
    assert_eq!(
        positions,
        vec![Sequence::new(1), Sequence::new(2), Sequence::new(3)],
        "every record kept the position it held"
    );

    // Done once: a second open finds nothing of the old shape and leaves
    // the store exactly as the first left it.
    drop(store);
    let reopened = Store::open(shared).unwrap();
    assert_eq!(
        read_format_version(Arc::clone(reopened.backend())).unwrap(),
        Some(FormatVersion::CURRENT)
    );
    assert_eq!(reopened.logs().unwrap(), vec![migrated]);
    assert_eq!(
        reopened
            .log_records(migrated, Sequence::new(1), 16)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn a_store_whose_history_predates_writers_does_not_report_itself_empty() {
    // Q-764, measured before it was written: a store written by
    // `0.0.6-beta` and opened by `0.3.0-beta` answered `--health` with
    // "well — committed to sequence 0" while `--backup` read every record
    // out of the same store. A store nobody has ever written answers that
    // same sentence, so the two states an operator most needs to tell apart
    // — nothing here, and everything here under a name this node does not
    // own — were one sentence, at the one moment an upgrade makes somebody
    // read it.
    //
    // The cause is not a lost record. `give_an_older_log_its_writer`
    // attributes what it rewrites to NOBODY, deliberately and correctly,
    // and this reports the node's OWN log, which is empty until this node
    // writes. Both halves are right and the sentence built from one of them
    // was not.
    let shared = backend();
    Store::open(Arc::clone(&shared)).unwrap();
    let home = Reach::Store;
    let mut batch = WriteBatch::new().put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::HOMED_LOG.encode(),
    );
    for sequence in 1_u64..=3 {
        let mut key = vec![KeyKind::LogEntry.tag()];
        key.extend_from_slice(&unqualified_home(home));
        key.extend_from_slice(&sequence.to_be_bytes());
        batch = batch.put(
            LogKey::keyspace(),
            Key::from(key),
            LogRecord::new(Vec::new()).encode(),
        );
    }
    let mut position = vec![KeyKind::AppliedPosition.tag()];
    position.extend_from_slice(&unqualified_home(home));
    batch = batch.put(
        AppliedPositionKey::keyspace(),
        Key::from(position),
        Sequence::new(3).encode(),
    );
    shared.apply(batch).unwrap();

    // Since ADR-0107 a log written before writers were named is the line's
    // log (`Writer::UNATTRIBUTED` is `Writer::LINE`), and health reports the
    // log this node's history is in — so the history is counted where it
    // is, and nothing is left "elsewhere" to need the second number.
    let held = Store::open(shared).unwrap().health().unwrap();
    assert_eq!(
        held.committed,
        Sequence::new(3),
        "the store is not empty, and the number an operator reads says so"
    );
    assert_eq!(held.elsewhere, None);
}

#[test]
fn a_store_nobody_has_written_holds_nothing_elsewhere() {
    // The control the test above needs to mean anything: an empty store
    // must not acquire a second number, or `elsewhere` would report history
    // in every store there is and stop distinguishing anything.
    let held = Store::open(backend()).unwrap().health().unwrap();
    assert_eq!(held.committed, Sequence::ZERO);
    assert_eq!(held.elsewhere, None);
}

/// A home as the nine bytes a key carried it in before writers were named.
///
/// Taken from the qualified encoding rather than written out by hand, which
/// is exact: the writer is a fixed-width suffix, so the leading bytes of a
/// qualified key ARE the unqualified one.
fn unqualified_home(home: Reach) -> Vec<u8> {
    AppliedPositionKey::new(LogId::unattributed(home))
        .encode()
        .into_bytes()
        .get(1..1 + REACH_LEN)
        .unwrap()
        .to_vec()
}

#[test]
fn a_log_written_before_it_had_homes_is_rewritten_into_the_store_home() {
    // The migration, end to end and against real bytes: a store standing in
    // the shape version 2 left — flat log keys and one singleton position —
    // is opened, and every record it held is readable afterwards at the
    // position it held, in the home one leader wrote it into.
    let shared = backend();
    Store::open(Arc::clone(&shared)).unwrap();
    let mut batch = WriteBatch::new().put(
        FormatVersionKey::keyspace(),
        FormatVersionKey.encode(),
        FormatVersion::new(2).encode(),
    );
    for sequence in 1_u64..=3 {
        // The old shape, written out here rather than built by a helper:
        // this test is the only thing left that knows it.
        let mut key = vec![KeyKind::LogEntry.tag()];
        key.extend_from_slice(&sequence.to_be_bytes());
        batch = batch.put(
            LogKey::keyspace(),
            Key::from(key),
            LogRecord::new(Vec::new()).encode(),
        );
    }
    batch = batch.put(
        AppliedPositionKey::keyspace(),
        Key::from(vec![KeyKind::AppliedPosition.tag()]),
        Sequence::new(3).encode(),
    );
    shared.apply(batch).unwrap();

    let store = Store::open(Arc::clone(&shared)).unwrap();
    assert_eq!(
        store.logs().unwrap(),
        vec![LogId::unattributed(Reach::Store)],
        "one leader wrote all of it, so the store's own log is its home — \
             and nobody recorded which node that leader was"
    );
    assert_eq!(
        store
            .committed_tail(LogId::unattributed(Reach::Store))
            .unwrap(),
        Sequence::new(3)
    );
    let positions: Vec<Sequence> = store
        .log_records(LogId::unattributed(Reach::Store), Sequence::new(1), 16)
        .unwrap()
        .into_iter()
        .map(|(sequence, _)| sequence)
        .collect();
    assert_eq!(
        positions,
        vec![Sequence::new(1), Sequence::new(2), Sequence::new(3)],
        "every record kept the position it held"
    );

    // And it is done once: a second open finds nothing of the old shape and
    // leaves the store exactly as the first left it.
    drop(store);
    let reopened = Store::open(shared).unwrap();
    assert_eq!(
        reopened
            .log_records(LogId::unattributed(Reach::Store), Sequence::new(1), 16)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn a_newer_on_disk_format_is_refused_rather_than_opened() {
    let shared = backend();
    let future = FormatVersion::new(FormatVersion::CURRENT.get().saturating_add(1));
    shared
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            future.encode(),
        ))
        .unwrap();

    let error = Store::open(shared).unwrap_err();
    assert_eq!(error.code(), "incompatible");
    assert!(!error.is_retryable());
    match error {
        Error::Encoding(inner) => {
            assert!(inner.to_string().contains("format version"), "{inner}");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_store_holding_data_without_a_format_stamp_is_refused_and_left_alone() {
    let shared = backend();
    let record = tessari_kv::Key::from(vec![KeyKind::Record.tag(), 1, 2, 3]);
    shared
        .apply(WriteBatch::new().put(
            tessari_kv::Keyspace::DATA,
            record.clone(),
            tessari_kv::Value::from(vec![1]),
        ))
        .unwrap();

    let error = Store::open(Arc::clone(&shared)).unwrap_err();
    assert_eq!(error.code(), "corruption", "{error}");
    match &error {
        Error::Encoding(tessari_encoding::Error::UnstampedStore { keyspace }) => {
            assert_eq!(*keyspace, "data");
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(
        read_format_version(Arc::clone(&shared)).unwrap(),
        None,
        "not stamped on the way to refusing"
    );
    assert!(
        shared
            .get(tessari_kv::Keyspace::DATA, &record)
            .unwrap()
            .is_some()
    );
}

#[test]
fn an_empty_store_is_a_new_one_and_is_stamped() {
    let shared = backend();
    Store::open(Arc::clone(&shared)).unwrap();
    assert_eq!(
        read_format_version(shared).unwrap(),
        Some(FormatVersion::CURRENT)
    );
}
