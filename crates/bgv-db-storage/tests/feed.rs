//! The change feed, and the one property that makes it worth having.
//!
//! It is a projection of the log rather than a mechanism of its own, so the
//! question these tests actually ask is whether the projection *is* the log:
//! same order, same commits, same content, and identical on a replica reading
//! the same records. A feed that had state of its own would have to be tested
//! for agreeing with the store; this one has to be tested for being derived.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bgv_db_encoding::encode_payload;
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::{Catalog, Change, ChangeKind, RecordAddress, Store, TableShape, Transaction};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value};

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
}

impl Fixture {
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(backend).unwrap();

        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "orders").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "users", TableShape::default())
            .unwrap();
        transaction.commit().unwrap();

        Self {
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
        }
    }

    fn at(&self, id: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(id),
        )
    }

    fn begin(&self) -> Transaction<'_> {
        self.store.begin().unwrap()
    }

    fn changes(&self) -> Vec<Change> {
        self.store
            .changes_since(Sequence::ZERO, 1024)
            .unwrap()
            .changes
    }
}

fn record(name: &str) -> Vec<u8> {
    encode_payload(&Value::Object(BTreeMap::from([(
        "name".to_owned(),
        Value::from(name),
    )])))
    .into_bytes()
}

/// The position a reader that has consumed `sequence` resumes from.
///
/// A reader's position is "what I have seen", so resuming means asking for what
/// follows it — the one arithmetic a subscriber does, written once here.
fn after(sequence: Sequence) -> Sequence {
    Sequence::new(sequence.get().saturating_add(1))
}

fn named(change: &Change) -> Option<String> {
    match &change.kind {
        ChangeKind::Written(Value::Object(fields)) => match fields.get("name") {
            Some(Value::String(name)) => Some(name.clone()),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn a_commit_of_two_records_is_two_changes_at_one_sequence() {
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.put(fixture.at("u2"), record("grace"));
    let at = transaction.commit().unwrap();

    let changes = fixture.changes();
    assert_eq!(changes.len(), 2, "{changes:?}");
    // One sequence, because they were one commit — which is what lets a
    // subscriber apply them as the unit they were written as.
    assert!(changes.iter().all(|change| change.sequence == at));
    assert_eq!(named(&changes[0]).as_deref(), Some("ada"));
    assert_eq!(named(&changes[1]).as_deref(), Some("grace"));
}

#[test]
fn a_delete_is_a_removal_and_a_write_carries_its_value() {
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.commit().unwrap();

    let mut transaction = fixture.begin();
    transaction.delete(fixture.at("u1"));
    transaction.commit().unwrap();

    let changes = fixture.changes();
    assert_eq!(changes.len(), 2);
    assert!(matches!(changes[0].kind, ChangeKind::Written(_)));
    assert_eq!(changes[1].kind, ChangeKind::Removed);
    assert_eq!(changes[1].id, RecordId::from("u1"));
}

#[test]
fn the_catalog_is_not_in_the_feed_and_the_records_beside_it_are() {
    // A catalog entry is an ordinary record in the system tenancy (ADR-0009),
    // which is what makes replication work and what a subscriber watching
    // `users` did not ask for.
    let fixture = Fixture::new();
    assert!(
        fixture.changes().is_empty(),
        "creating a namespace, a database and a table produced changes"
    );

    // A definition and a write in one commit: the write is in the feed, the
    // definition is not.
    let mut transaction = fixture.begin();
    Catalog::new(&mut transaction)
        .create_index(
            fixture.table,
            "by_name",
            vec![bgv_db_types::Path::field("name")],
            false,
        )
        .unwrap();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.commit().unwrap();

    let changes = fixture.changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(named(&changes[0]).as_deref(), Some("ada"));
}

#[test]
fn reading_from_a_sequence_returns_what_follows_it() {
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    let first = transaction.commit().unwrap();

    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u2"), record("grace"));
    transaction.commit().unwrap();

    let later = fixture.store.changes_since(after(first), 1024).unwrap();
    assert_eq!(later.changes.len(), 1);
    assert_eq!(named(&later.changes[0]).as_deref(), Some("grace"));

    // And a reader that has caught up sees nothing rather than the last change
    // again.
    let tail = fixture.store.committed_tail().unwrap();
    let caught_up = fixture.store.changes_since(after(tail), 1024).unwrap();
    assert!(caught_up.changes.is_empty());
    // …and it is told to stay where it is rather than being moved backwards.
    assert_eq!(caught_up.next, after(tail));
}

#[test]
fn a_replicas_feed_is_the_leaders_feed() {
    // The property the whole design rests on: no state of its own, so the same
    // log projects to the same changes wherever it is read.
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.put(fixture.at("u2"), record("grace"));
    transaction.commit().unwrap();

    let mut transaction = fixture.begin();
    transaction.delete(fixture.at("u1"));
    transaction.commit().unwrap();

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    for (sequence, log) in fixture.store.log_records(Sequence::ZERO, 1024).unwrap() {
        replica.apply_record(sequence, &log).unwrap();
    }

    assert_eq!(
        replica.changes_since(Sequence::ZERO, 1024).unwrap().changes,
        fixture.changes()
    );
}

#[test]
fn a_record_written_and_deleted_in_one_commit_appears_as_the_log_carries_it() {
    // The feed reports the log, not a reconstruction of what the writer meant.
    // A transaction folds its own writes, so what reaches the log is one
    // mutation — and the feed says exactly that.
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.delete(fixture.at("u1"));
    transaction.commit().unwrap();

    let changes = fixture.changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].kind, ChangeKind::Removed);
}

#[test]
fn a_commit_that_changes_no_records_still_moves_the_reader_forward() {
    // A commit that only touched the catalog produces no changes. A reader given
    // only a list could not tell that from "nothing has happened", and would ask
    // for the same records forever — so the answer says where it reached.
    let fixture = Fixture::new();
    let answer = fixture.store.changes_since(Sequence::ZERO, 1024).unwrap();
    assert!(answer.changes.is_empty(), "the catalog is not in the feed");
    assert!(
        answer.next > Sequence::ZERO,
        "a reader that saw no changes still has to advance"
    );

    // Resuming from there sees the next write and not the catalog again.
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.commit().unwrap();
    let next = fixture.store.changes_since(answer.next, 1024).unwrap();
    assert_eq!(next.changes.len(), 1);
}

#[test]
fn a_limit_bounds_commits_and_never_splits_one() {
    // A subscriber applies a commit as the unit it was written as, so a feed
    // that returned half of one would hand out a state that never existed.
    let fixture = Fixture::new();
    for round in 0..3_u8 {
        let mut transaction = fixture.begin();
        transaction.put(fixture.at(&format!("a{round}")), record("ada"));
        transaction.put(fixture.at(&format!("b{round}")), record("grace"));
        transaction.commit().unwrap();
    }

    // Two log records — and the first is the fixture's own catalog commit, which
    // produces no changes at all. So a limit counts records, not changes, and a
    // reader has to be told where it reached rather than inferring it.
    let first = fixture.store.changes_since(Sequence::ZERO, 2).unwrap();
    assert_eq!(first.changes.len(), 2, "one data commit, whole");
    assert_eq!(first.changes[0].sequence, first.changes[1].sequence);

    let second = fixture.store.changes_since(first.next, 2).unwrap();
    assert_eq!(second.changes.len(), 4, "two commits, both whole");
    let sequences: Vec<_> = second
        .changes
        .iter()
        .map(|change| change.sequence)
        .collect();
    assert_eq!(sequences[0], sequences[1]);
    assert_eq!(sequences[2], sequences[3]);
    assert_ne!(sequences[1], sequences[2]);
}
