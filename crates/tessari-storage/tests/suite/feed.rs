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
    Catalog, Change, ChangeKind, IndexShape, RecordAddress, Store, Subject, Subscription,
    TableShape, Transaction, Watch,
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
            .changes_since(
                self.store.own_log(crate::FIXTURE_HOME).unwrap(),
                Sequence::ZERO,
                1024,
            )
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
        .changes_since(
            fixture.store.own_log(tessari_types::Reach::Store).unwrap(),
            Sequence::ZERO,
            1024,
        )
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
        .changes_since(
            fixture.store.own_log(crate::FIXTURE_HOME).unwrap(),
            after(first),
            1024,
        )
        .unwrap();
    assert_eq!(later.changes.len(), 1);
    assert_eq!(named(&later.changes[0]).as_deref(), Some("grace"));

    // And a reader that has caught up sees nothing rather than the last change
    // again.
    let tail = fixture
        .store
        .committed_tail(fixture.store.own_log(crate::FIXTURE_HOME).unwrap())
        .unwrap();
    let caught_up = fixture
        .store
        .changes_since(
            fixture.store.own_log(crate::FIXTURE_HOME).unwrap(),
            after(tail),
            1024,
        )
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
            .changes_since(crate::fixture_log(&fixture.store), Sequence::ZERO, 1024)
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
        .changes_since(
            fixture.store.own_log(tessari_types::Reach::Store).unwrap(),
            Sequence::ZERO,
            1024,
        )
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
        .changes_since(
            fixture.store.own_log(crate::FIXTURE_HOME).unwrap(),
            Sequence::ZERO,
            1024,
        )
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
        .changes_since(
            fixture.store.own_log(crate::FIXTURE_HOME).unwrap(),
            Sequence::ZERO,
            2,
        )
        .unwrap();
    assert_eq!(first.changes.len(), 4, "two data commits, both whole");
    assert_eq!(first.changes[0].sequence, first.changes[1].sequence);

    let second = fixture
        .store
        .changes_since(
            fixture.store.own_log(crate::FIXTURE_HOME).unwrap(),
            first.next,
            2,
        )
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

    let mut subscription = Subscription::new(
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::default(),
    );
    let mut received = Vec::new();
    loop {
        let batch = subscription.poll(&fixture.store, 2).unwrap();
        let before = subscription.position();
        received.extend(batch);
        if subscription.position() == before && received.len() >= 5 {
            break;
        }
        if subscription.position()
            > fixture
                .store
                .committed_tail(fixture.store.own_log(crate::FIXTURE_HOME).unwrap())
                .unwrap()
        {
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
        crate::fixture_log(&fixture.store),
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

    let mut subscription = Subscription::new(
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::default(),
    );
    // Take a few, then decide to be current rather than complete.
    let first = subscription.poll(&fixture.store, 3).unwrap();
    let tail = fixture
        .store
        .committed_tail(fixture.store.own_log(crate::FIXTURE_HOME).unwrap())
        .unwrap();
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
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::table(fixture.table),
    );
    let tail = fixture
        .store
        .committed_tail(fixture.store.own_log(crate::FIXTURE_HOME).unwrap())
        .unwrap();
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

    let mut subscription = Subscription::new(
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::default(),
    );
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

    let mut ahead = Subscription::new(
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::default(),
    );
    ahead.poll(&fixture.store, 1024).unwrap();

    let mut behind = Subscription::new(
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::default(),
    );
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
    let mut subscription = Subscription::new(
        crate::fixture_log(&fixture.store),
        Sequence::ZERO,
        Watch::default(),
    );
    subscription.poll(&fixture.store, 1024).unwrap();
    write(&fixture, fixture.table, "u2", "grace");

    let replica_backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let replica = Store::open(Arc::clone(&replica_backend)).unwrap();
    crate::replay(&fixture.store, &replica);

    let resumed = subscription.poll(&replica, 1024).unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(named(&resumed[0]).as_deref(), Some("grace"));
}

// --- One object's history -------------------------------------------------
//
// A history is the same projection read backwards and filtered to one record.
// The properties worth testing are therefore not "does it find events" — the
// feed tests above already establish the projection — but the three ways a
// history can lie: it can return somebody else's rows, it can return them in an
// order that reads as the wrong story, and it can show a partial answer as a
// whole one.

impl Fixture {
    /// The history of one record in this fixture's table.
    fn history(&self, id: &str, limit: usize) -> tessari_storage::History {
        self.store
            .history_of(
                self.store.own_log(crate::FIXTURE_HOME).unwrap(),
                &Subject::new(
                    self.namespace,
                    self.database,
                    self.table,
                    RecordId::from(id),
                ),
                limit,
            )
            .unwrap()
    }
}

#[test]
fn a_records_history_is_its_own_writes_newest_first() {
    let fixture = Fixture::new();
    for name in ["ada", "grace", "edith"] {
        let mut transaction = fixture.begin();
        transaction.put(fixture.at("u1"), record(name));
        transaction.commit().unwrap();
    }

    let history = fixture.history("u1", 10);
    let names: Vec<Option<String>> = history.events.iter().map(named).collect();
    // Newest first. A timeline drawn from this renders top-down, so the order
    // is not presentation: reversed, it tells the opposite story about what the
    // record most recently became.
    assert_eq!(
        names,
        vec![
            Some("edith".to_owned()),
            Some("grace".to_owned()),
            Some("ada".to_owned())
        ],
        "{history:?}"
    );
    assert!(
        history
            .events
            .windows(2)
            .all(|pair| pair[0].sequence > pair[1].sequence),
        "sequences are not strictly descending: {history:?}"
    );
    assert!(
        history.complete,
        "a three-record log was reported truncated"
    );
}

#[test]
fn a_history_holds_no_other_records_writes() {
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.put(fixture.at("u2"), record("grace"));
    transaction.commit().unwrap();

    // The two were written in ONE commit, so they share a sequence. A filter
    // that keyed on the commit rather than the address would return both, and
    // the answer would look entirely plausible.
    let history = fixture.history("u1", 10);
    assert_eq!(history.events.len(), 1, "{history:?}");
    assert_eq!(history.events[0].id, RecordId::from("u1"));
    assert_eq!(named(&history.events[0]).as_deref(), Some("ada"));
}

#[test]
fn a_removal_is_in_the_history_and_a_record_that_never_existed_has_none() {
    let fixture = Fixture::new();
    let mut transaction = fixture.begin();
    transaction.put(fixture.at("u1"), record("ada"));
    transaction.commit().unwrap();
    let mut transaction = fixture.begin();
    transaction.delete(fixture.at("u1"));
    transaction.commit().unwrap();

    let history = fixture.history("u1", 10);
    assert_eq!(history.events.len(), 2, "{history:?}");
    assert_eq!(history.events[0].kind, ChangeKind::Removed);

    // Empty and complete, which is a different claim from empty and truncated:
    // one says nothing ever happened, the other says nothing was found in what
    // was read.
    let absent = fixture.history("nobody", 10);
    assert!(absent.events.is_empty(), "{absent:?}");
    assert!(absent.complete);
}

#[test]
fn a_history_cut_short_by_its_limit_still_says_it_is_complete() {
    let fixture = Fixture::new();
    for name in ["ada", "grace", "edith"] {
        let mut transaction = fixture.begin();
        transaction.put(fixture.at("u1"), record(name));
        transaction.commit().unwrap();
    }

    // `limit` and `complete` answer different questions, and conflating them is
    // the bug this guards. The caller asked for two; the walk still reached the
    // beginning of the log, so the LOG is not truncated even though the ANSWER
    // is. A console showing "there may be more" here would be crying wolf on
    // every screen that paginates.
    let history = fixture.history("u1", 2);
    assert_eq!(history.events.len(), 2, "{history:?}");
    assert_eq!(named(&history.events[0]).as_deref(), Some("edith"));
    assert!(history.complete, "the limit was mistaken for truncation");
}

#[test]
fn the_walk_is_bounded_and_reports_what_it_read() {
    let fixture = Fixture::new();
    // The middle commit touches a DIFFERENT record, so the walk reads a log
    // record that yields nothing for `u1`. That gap is the whole point of
    // reporting the cost: a history of two events that cost three records read
    // is cheap, and the same two events after two thousand records read is a
    // screen nobody should be drawing. (The fixture's own catalog commit is not
    // in this count — a catalog row homes in the system tenancy and never enters
    // this log, which the first cut of this test assumed wrongly.)
    for (id, name) in [("u1", "ada"), ("u2", "grace"), ("u1", "edith")] {
        let mut transaction = fixture.begin();
        transaction.put(fixture.at(id), record(name));
        transaction.commit().unwrap();
    }

    let history = fixture.history("u1", 10);
    assert_eq!(history.events.len(), 2, "{history:?}");
    assert_eq!(history.walked, 3, "{history:?}");
    assert!(
        history.walked > history.events.len(),
        "the walk claims to have read no more records than it returned events: {history:?}"
    );

    // The reverse reader underneath is what bounds it, and its bound is exact.
    let newest = fixture
        .store
        .log_records_newest_first(fixture.store.own_log(crate::FIXTURE_HOME).unwrap(), 1)
        .unwrap();
    assert_eq!(newest.len(), 1, "the reverse read ignored its limit");
    let all = fixture
        .store
        .log_records_newest_first(fixture.store.own_log(crate::FIXTURE_HOME).unwrap(), 1024)
        .unwrap();
    assert_eq!(
        newest[0].0, all[0].0,
        "the bounded read did not start at the newest record"
    );
    assert!(
        all.windows(2).all(|pair| pair[0].0 > pair[1].0),
        "the reverse read is not newest-first"
    );
}
