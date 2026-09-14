//! The maintained record count agrees with the records, after any run of writes.
//!
//! The planner reads this number to decide whether an index is worth using, and
//! a number that decides an access path can be stale or missing without costing
//! a wrong answer — so nothing about a query will ever report that it drifted.
//! That is exactly why it needs a sweep: the only way this breaks is silently.
//!
//! The three ways it can go wrong are each a different mistake:
//!
//! - **An update counted as an arrival.** Writing over a record that is already
//!   there is a replacement, and a counter that added would climb without bound
//!   on a table whose records are edited.
//! - **A delete of something absent counted as a departure.** Removing a record
//!   that was never there is not a loss, and a counter that subtracted would
//!   sink below the truth.
//! - **A count that never came back down.** This is the failure the store
//!   already had: the record *sequence* is an identity allocator, so a churned
//!   table reads far too large under it and a table written with explicit ids
//!   reads zero. Counting a delete is the whole difference between the two
//!   numbers, and it is asserted here rather than assumed.
//!
//! The expected number is re-derived from a scan of the table rather than from
//! the counter's own arithmetic. Comparing a function against itself proves
//! nothing.
//!
//! The workload is pseudo-random and **deterministic** — a fixed seed and a
//! multiplicative generator — so a failure is reproducible from the seed rather
//! than being a story about a run nobody can repeat.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, RecordAddress, Store, TableShape};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

/// The seed the workload runs from. Printed by every failing assertion.
const SEED: u64 = 0x00c0_ffee_0bad_f00d;
/// How many workload steps to run.
const STEPS: u64 = 500;
/// How many distinct records the workload writes over.
///
/// Far fewer than the steps, so most writes land on a record that is already
/// there — which is the update-counted-as-an-arrival case, and it is the one a
/// short workload would miss.
const RECORDS: u64 = 30;

/// A multiplicative congruential generator, so the workload is a function of
/// the seed and nothing else.
struct Rolls(u64);

impl Rolls {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }
}

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    other: TableId,
}

impl Fixture {
    /// Two tables in one database, so the counters are proved to be **per
    /// table** rather than one number the whole store shares.
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(backend).unwrap();

        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "orders").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "people", TableShape::default())
            .unwrap();
        let other = catalog
            .create_table(namespace.id, database.id, "places", TableShape::default())
            .unwrap();
        transaction.commit().unwrap();

        Self {
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            other: other.id,
        }
    }

    fn at(&self, table: TableId, n: u64) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            table,
            RecordId::from(format!("r{n:03}")),
        )
    }

    /// What the counter says, or `None` when nothing has been written.
    fn counted(&self, table: TableId) -> Option<u64> {
        let mut transaction = self.store.begin().unwrap();
        Catalog::new(&mut transaction).record_count(table).unwrap()
    }

    /// What is actually there, established independently of the counter.
    fn present(&self, table: TableId) -> u64 {
        let transaction = self.store.begin().unwrap();
        let found = transaction
            .scan_table(self.namespace, self.database, table)
            .unwrap();
        u64::try_from(found.len()).unwrap()
    }

    fn agrees(&self, table: TableId, after: u64) {
        let present = self.present(table);
        assert_eq!(
            self.counted(table),
            Some(present),
            "the count disagrees with the records after step {after} of seed {SEED:#x}"
        );
    }
}

#[test]
fn a_table_nobody_has_written_has_no_count_rather_than_a_zero() {
    // The two are worth telling apart. "No estimate" sends the planner back to
    // the behaviour it had before counts existed; a zero would tell it that
    // every index beats a scan of nothing.
    let fixture = Fixture::new();
    assert_eq!(fixture.counted(fixture.table), None);
    assert_eq!(fixture.present(fixture.table), 0);
}

#[test]
fn an_arrival_a_replacement_and_a_departure_count_once_each() {
    let fixture = Fixture::new();
    let address = fixture.at(fixture.table, 1);

    let mut transaction = fixture.store.begin().unwrap();
    transaction.put(
        address.clone(),
        encode_payload(&Value::from("first")).into_bytes(),
    );
    transaction.commit().unwrap();
    assert_eq!(fixture.counted(fixture.table), Some(1));

    // A replacement, which is the case a counter that only ever adds gets
    // wrong — and gets wrong in a way no read would ever report.
    let mut transaction = fixture.store.begin().unwrap();
    transaction.put(
        address.clone(),
        encode_payload(&Value::from("second")).into_bytes(),
    );
    transaction.commit().unwrap();
    assert_eq!(fixture.counted(fixture.table), Some(1));

    let mut transaction = fixture.store.begin().unwrap();
    transaction.delete(address.clone());
    transaction.commit().unwrap();
    assert_eq!(
        fixture.counted(fixture.table),
        Some(0),
        "a delete has to bring the count down — that is the whole difference \
         between this number and the record sequence"
    );

    // A delete of something already gone is not a departure. A counter that
    // subtracted here would sink below the truth and stay there.
    let mut transaction = fixture.store.begin().unwrap();
    transaction.delete(address);
    transaction.commit().unwrap();
    assert_eq!(fixture.counted(fixture.table), Some(0));
}

#[test]
fn one_commit_carrying_many_mutations_counts_all_of_them() {
    // The deltas are folded per table inside one commit, so a batch that adds
    // three and removes one has to move the count by two — not by one, and not
    // by four.
    let fixture = Fixture::new();
    let mut transaction = fixture.store.begin().unwrap();
    for n in 0..4 {
        transaction.put(
            fixture.at(fixture.table, n),
            encode_payload(&Value::from(format!("v{n}"))).into_bytes(),
        );
    }
    transaction.commit().unwrap();
    assert_eq!(fixture.counted(fixture.table), Some(4));

    let mut transaction = fixture.store.begin().unwrap();
    for n in 4..7 {
        transaction.put(
            fixture.at(fixture.table, n),
            encode_payload(&Value::from(format!("v{n}"))).into_bytes(),
        );
    }
    transaction.delete(fixture.at(fixture.table, 0));
    transaction.commit().unwrap();
    assert_eq!(fixture.counted(fixture.table), Some(6));
    assert_eq!(fixture.present(fixture.table), 6);
}

#[test]
fn two_tables_keep_two_counts() {
    let fixture = Fixture::new();
    let mut transaction = fixture.store.begin().unwrap();
    transaction.put(
        fixture.at(fixture.table, 1),
        encode_payload(&Value::from(1)).into_bytes(),
    );
    transaction.put(
        fixture.at(fixture.other, 1),
        encode_payload(&Value::from(1)).into_bytes(),
    );
    transaction.put(
        fixture.at(fixture.other, 2),
        encode_payload(&Value::from(2)).into_bytes(),
    );
    transaction.commit().unwrap();

    assert_eq!(fixture.counted(fixture.table), Some(1));
    assert_eq!(fixture.counted(fixture.other), Some(2));
}

#[test]
fn the_count_and_the_records_agree_after_an_arbitrary_workload() {
    let fixture = Fixture::new();
    let mut rolls = Rolls(SEED);

    for step in 0..STEPS {
        let n = rolls.below(RECORDS);
        let address = fixture.at(fixture.table, n);
        let mut transaction = fixture.store.begin().unwrap();
        // Weighted towards writing, so the table grows before it churns and
        // both directions of the counter are exercised over a non-empty table.
        if rolls.below(3) == 0 {
            transaction.delete(address);
        } else {
            transaction.put(
                address,
                encode_payload(&Value::from(format!("v{step}"))).into_bytes(),
            );
        }
        transaction.commit().unwrap();
        fixture.agrees(fixture.table, step);
    }

    // The workload has to have done something, or the assertion above passed on
    // an empty table five hundred times.
    assert!(
        fixture.present(fixture.table) > 0,
        "the workload left nothing behind, so it proved nothing"
    );
}

#[test]
fn a_replica_replaying_the_log_reaches_the_same_count() {
    // The reason the count is derived from the log record rather than added to
    // the leader's batch. A follower that carried the records and none of the
    // counts would have a planner choosing a different access path for the same
    // query, with nothing anywhere in an error state.
    let fixture = Fixture::new();
    for n in 0..5 {
        let mut transaction = fixture.store.begin().unwrap();
        transaction.put(
            fixture.at(fixture.table, n),
            encode_payload(&Value::from(format!("v{n}"))).into_bytes(),
        );
        transaction.commit().unwrap();
    }
    let mut transaction = fixture.store.begin().unwrap();
    transaction.delete(fixture.at(fixture.table, 0));
    transaction.commit().unwrap();

    let follower = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    crate::replay(&fixture.store, &follower);

    let mut transaction = follower.begin().unwrap();
    assert_eq!(
        Catalog::new(&mut transaction)
            .record_count(fixture.table)
            .unwrap(),
        Some(4),
        "the follower reached a different count from the leader"
    );
}
