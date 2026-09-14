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

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{
    Catalog, Change, ChangeKind, IndexShape, RecordAddress, Store, Subscription, TableShape,
    Transaction, Watch,
};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value};

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
            .changes_since(crate::FIXTURE_HOME, Sequence::ZERO, 1024)
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
            vec![tessari_types::Path::field("name")],
            IndexShape::default(),
        )
        .unwrap();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.commit().unwrap();

    // The STORE's log, because a record's log is the JOIN of what its mutations
    // are carried to (Q-620) and this commit carries both: an index definition,
    // which goes everywhere, and a row, which goes to one database. The join is
    // the store — so a commit that touches the catalog AND a record leaves the
    // range's own feed entirely, which is the consequence of the join worth
    // knowing before a subscriber is written against it.
    let changes = fixture
        .store
        .changes_since(tessari_types::Reach::Store, Sequence::ZERO, 1024)
        .unwrap()
        .changes;
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

    let later = fixture
        .store
        .changes_since(crate::FIXTURE_HOME, after(first), 1024)
        .unwrap();
    assert_eq!(later.changes.len(), 1);
    assert_eq!(named(&later.changes[0]).as_deref(), Some("grace"));

    // And a reader that has caught up sees nothing rather than the last change
    // again.
    let tail = fixture.store.committed_tail(crate::FIXTURE_HOME).unwrap();
    let caught_up = fixture
        .store
        .changes_since(crate::FIXTURE_HOME, after(tail), 1024)
        .unwrap();
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
    crate::replay(&fixture.store, &replica);

    assert_eq!(
        replica
            .changes_since(crate::FIXTURE_HOME, Sequence::ZERO, 1024)
            .unwrap()
            .changes,
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
    // The STORE's log, because that is where a catalog commit is filed: a
    // namespace, a database and a table are carried everywhere, so their record
    // homes at the store and not in the range they describe (Q-620).
    let answer = fixture
        .store
        .changes_since(tessari_types::Reach::Store, Sequence::ZERO, 1024)
        .unwrap();
    assert!(answer.changes.is_empty(), "the catalog is not in the feed");
    assert!(
        answer.next > Sequence::ZERO,
        "a reader that saw no changes still has to advance"
    );

    // Resuming from there sees the next write and not the catalog again — in
    // the range's own log, which is where a plain write is filed and where the
    // catalog commit above never was.
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.commit().unwrap();
    let next = fixture
        .store
        .changes_since(crate::FIXTURE_HOME, Sequence::ZERO, 1024)
        .unwrap();
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

    // Two log records, both data commits — the fixture's own catalog commit is
    // filed in the store's log now and is not in this one at all (Q-620). So a
    // limit counts records, not changes, and a reader has to be told where it
    // reached rather than inferring it.
    let first = fixture
        .store
        .changes_since(crate::FIXTURE_HOME, Sequence::ZERO, 2)
        .unwrap();
    assert_eq!(first.changes.len(), 4, "two data commits, both whole");
    assert_eq!(first.changes[0].sequence, first.changes[1].sequence);

    let second = fixture
        .store
        .changes_since(crate::FIXTURE_HOME, first.next, 2)
        .unwrap();
    assert_eq!(second.changes.len(), 2, "the third commit, whole");
    // The two halves of one commit share a sequence, and the commit before them
    // does not — which is the property a limit must never break.
    let sequences: Vec<_> = first
        .changes
        .iter()
        .chain(second.changes.iter())
        .map(|change| change.sequence)
        .collect();
    assert_eq!(sequences[0], sequences[1]);
    assert_eq!(sequences[2], sequences[3]);
    assert_eq!(sequences[4], sequences[5]);
    assert_ne!(sequences[1], sequences[2]);
    assert_ne!(sequences[3], sequences[4]);
}

// ------------------------------------------------------------- subscriptions

/// A second table, so a filtered subscription has something to ignore.
fn other_table(fixture: &Fixture) -> TableId {
    let mut transaction = fixture.begin();
    let table = Catalog::new(&mut transaction)
        .create_table(
            fixture.namespace,
            fixture.database,
            "notes",
            TableShape::default(),
        )
        .unwrap();
    transaction.commit().unwrap();
    table.id
}

fn write(fixture: &Fixture, table: TableId, id: &str, name: &str) {
    let mut transaction = fixture.begin();
    transaction.put(
        RecordAddress::new(
            fixture.namespace,
            fixture.database,
            table,
            RecordId::from(id),
        ),
        record(name),
    );
    transaction.commit().unwrap();
}

#[test]
fn a_subscription_receives_every_change_across_several_polls() {
    let fixture = Fixture::new();
    for round in 0..5_u8 {
        write(&fixture, fixture.table, &format!("u{round}"), "ada");
    }

    let mut subscription = Subscription::new(crate::FIXTURE_HOME, Sequence::ZERO, Watch::default());
    let mut received = Vec::new();
    loop {
        let batch = subscription.poll(&fixture.store, 2).unwrap();
        let before = subscription.position();
        received.extend(batch);
        if subscription.position() == before && received.len() >= 5 {
            break;
        }
        if subscription.position() > fixture.store.committed_tail(crate::FIXTURE_HOME).unwrap() {
            break;
        }
    }
    assert_eq!(received, fixture.changes());
    assert_eq!(subscription.delivered(), 5);
    assert_eq!(subscription.dropped(), 0);
}

#[test]
fn a_filtered_subscription_receives_only_its_table_and_still_advances() {
    // The failure a filter invites: a subscriber watching one table stalling on
    // a run of writes to another, because it only advanced over what matched.
    let fixture = Fixture::new();
    let notes = other_table(&fixture);
    for round in 0..4_u8 {
        write(&fixture, notes, &format!("n{round}"), "noise");
    }
    write(&fixture, fixture.table, "u1", "ada");

    let mut subscription = Subscription::new(
        crate::FIXTURE_HOME,
        Sequence::ZERO,
        Watch::table(fixture.table),
    );
    let mut received = Vec::new();
    for _ in 0..8 {
        received.extend(subscription.poll(&fixture.store, 1).unwrap());
    }
    assert_eq!(received.len(), 1, "{received:?}");
    assert_eq!(named(&received[0]).as_deref(), Some("ada"));
    assert_eq!(subscription.delivered(), 1);
}

#[test]
fn what_was_delivered_plus_what_was_dropped_is_what_was_emitted() {
    // G001's C4, stated as an identity rather than as a feeling.
    let fixture = Fixture::new();
    for round in 0..10_u8 {
        write(&fixture, fixture.table, &format!("u{round}"), "ada");
    }
    let emitted = u64::try_from(fixture.changes().len()).expect("a count");
    assert_eq!(emitted, 10);

    let mut subscription = Subscription::new(crate::FIXTURE_HOME, Sequence::ZERO, Watch::default());
    // Take a few, then decide to be current rather than complete.
    let first = subscription.poll(&fixture.store, 3).unwrap();
    let tail = fixture.store.committed_tail(crate::FIXTURE_HOME).unwrap();
    let skipped = subscription.skip_to(&fixture.store, after(tail)).unwrap();
    let rest = subscription.poll(&fixture.store, 1024).unwrap();

    assert!(rest.is_empty(), "the skip should have reached the end");
    assert_eq!(
        subscription.delivered() + subscription.dropped(),
        emitted,
        "delivered {} + dropped {} != emitted {emitted}",
        subscription.delivered(),
        subscription.dropped()
    );
    assert_eq!(
        subscription.delivered(),
        u64::try_from(first.len()).expect("a count")
    );
    assert_eq!(skipped, subscription.dropped());
    assert!(skipped > 0, "the skip must be able to lose something");
}

#[test]
fn a_skip_counts_only_what_the_subscription_watches() {
    let fixture = Fixture::new();
    let notes = other_table(&fixture);
    for round in 0..3_u8 {
        write(&fixture, fixture.table, &format!("u{round}"), "ada");
        write(&fixture, notes, &format!("n{round}"), "noise");
    }

    let mut subscription = Subscription::new(
        crate::FIXTURE_HOME,
        Sequence::ZERO,
        Watch::table(fixture.table),
    );
    let tail = fixture.store.committed_tail(crate::FIXTURE_HOME).unwrap();
    let skipped = subscription.skip_to(&fixture.store, after(tail)).unwrap();
    assert_eq!(
        skipped, 3,
        "the other table's writes are not this one's loss"
    );
}

#[test]
fn a_skip_backwards_does_nothing_and_a_poll_after_one_does_not_repeat() {
    let fixture = Fixture::new();
    for round in 0..4_u8 {
        write(&fixture, fixture.table, &format!("u{round}"), "ada");
    }

    let mut subscription = Subscription::new(crate::FIXTURE_HOME, Sequence::ZERO, Watch::default());
    let taken = subscription.poll(&fixture.store, 3).unwrap();
    let reached = subscription.position();

    assert_eq!(
        subscription
            .skip_to(&fixture.store, Sequence::ZERO)
            .unwrap(),
        0
    );
    assert_eq!(
        subscription.position(),
        reached,
        "a skip backwards moved it"
    );

    let after_skip = subscription.poll(&fixture.store, 1024).unwrap();
    for change in &after_skip {
        assert!(!taken.contains(change), "a change was delivered twice");
    }
}

#[test]
fn two_subscriptions_at_different_positions_do_not_interfere() {
    // The store holds no registry of subscribers, so there is nothing for them
    // to share and nothing to clean up when one disappears.
    let fixture = Fixture::new();
    for round in 0..4_u8 {
        write(&fixture, fixture.table, &format!("u{round}"), "ada");
    }

    let mut ahead = Subscription::new(crate::FIXTURE_HOME, Sequence::ZERO, Watch::default());
    ahead.poll(&fixture.store, 1024).unwrap();

    let mut behind = Subscription::new(crate::FIXTURE_HOME, Sequence::ZERO, Watch::default());
    let all = behind.poll(&fixture.store, 1024).unwrap();

    assert_eq!(all, fixture.changes());
    assert_eq!(ahead.delivered(), behind.delivered());
}

#[test]
fn a_position_carries_to_another_store_holding_the_same_log() {
    // A subscription is a value the caller holds, and its position is a
    // sequence — so a restarted process, or a replica, resumes from exactly
    // where it stopped.
    let fixture = Fixture::new();
    write(&fixture, fixture.table, "u1", "ada");
    let mut subscription = Subscription::new(crate::FIXTURE_HOME, Sequence::ZERO, Watch::default());
    subscription.poll(&fixture.store, 1024).unwrap();
    write(&fixture, fixture.table, "u2", "grace");

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    crate::replay(&fixture.store, &replica);

    let resumed = subscription.poll(&replica, 1024).unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(named(&resumed[0]).as_deref(), Some("grace"));
}
