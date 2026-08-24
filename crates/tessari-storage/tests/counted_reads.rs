//! What a read actually costs the backend, counted.
//!
//! Every other test in this suite asserts what a read *answers*. These assert
//! what it *asks for*, because the two are independent: a bounded descending
//! read that walked the whole index would answer correctly, report `ordered`,
//! and pass every existing test — the cost is invisible to all of them.
//!
//! # Why a counting backend rather than a returned statistic
//!
//! The count is taken at the [`KvBackend`] boundary, which is where the work is
//! actually issued. A number the read computed about itself is a number that
//! agrees with the read by construction; this one is taken by the thing being
//! asked. It also costs the production types nothing — no counter to maintain,
//! no statistic to keep truthful.
//!
//! Two quantities, deliberately separate:
//!
//! - **entries returned** — how many rows the scans handed back. This is what a
//!   bounded read promises to keep proportional to its bound. Counting rows
//!   rather than calls is what makes the promise testable: one scan that
//!   returned the whole index is a single call and a linear cost.
//! - **round trips** — how many times the backend was asked anything at all.
//!
//! # Reading a record is a scan, not a `get`
//!
//! Worth stating because the first version of this file got it wrong and
//! measured **one** backend `get` for two thousand records. Records are
//! versioned, so reading one means finding the newest version at or below the
//! transaction's snapshot — `first_in_range`, a scan bounded to one row. A
//! `get` is a point lookup on an exact key and almost nothing on the read path
//! uses one.
//!
//! So the round-trip count is the **scan** count, and an instrument that
//! counted `get`s would have reported a flattering number about work that was
//! never issued that way. That is the failure mode of counting at the wrong
//! boundary: it does not raise, it just agrees with you.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_constants::ORDERED_SCAN_BATCH_ENTRIES;
use tessari_encoding::encode_payload;
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_storage::{Catalog, IndexDefinition, IndexShape, RecordAddress, Store, TableShape};
use tessari_types::{DatabaseId, NamespaceId, Path, RecordId, TableId, Value};

/// A backend that answers exactly as the one beneath it and says what it was
/// asked.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    entries: AtomicUsize,
    scans: AtomicUsize,
    point_reads: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryBackend::new()),
            entries: AtomicUsize::new(0),
            scans: AtomicUsize::new(0),
            point_reads: AtomicUsize::new(0),
        })
    }

    /// Start counting from here, so a fixture's own writes are not the answer.
    fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
        self.scans.store(0, Ordering::Relaxed);
        self.point_reads.store(0, Ordering::Relaxed);
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    /// Every question put to the backend: a scan call or a point lookup.
    fn round_trips(&self) -> usize {
        self.scans
            .load(Ordering::Relaxed)
            .saturating_add(self.point_reads.load(Ordering::Relaxed))
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<KvValue>> {
        self.point_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, KvValue)>> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        let found = self.inner.scan(request)?;
        self.entries.fetch_add(found.len(), Ordering::Relaxed);
        Ok(found)
    }

    /// Counted as **one** round trip however many ranges it carries.
    ///
    /// That is the claim being measured, so it has to be delegated rather than
    /// defaulted. The trait's default answers a batched call by making the
    /// un-batched ones, and taken here it would count one round trip per range
    /// and report the batching as having changed nothing — a wrapper that does
    /// not do what the backends below it do measures the wrapper.
    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, KvValue)>>> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        let found = self.inner.first_of_each(keyspace, ranges)?;
        let returned = found.iter().filter(|pair| pair.is_some()).count();
        self.entries.fetch_add(returned, Ordering::Relaxed);
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.inner.apply(batch)
    }
}

struct Fixture {
    counting: Arc<Counting>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    index: IndexDefinition,
}

impl Fixture {
    fn new() -> Self {
        let counting = Counting::new();
        let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "shop").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "users", TableShape::default())
            .unwrap();
        let index = catalog
            .create_index(
                table.id,
                "by_joined",
                vec![Path::field("joined")],
                IndexShape {
                    unique: false,
                    search: false,
                    spatial: false,
                    vector: None,
                },
            )
            .unwrap();
        transaction.commit().unwrap();
        Self {
            counting,
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            index,
        }
    }

    /// `count` records, each holding a distinct `joined` value.
    fn write(&self, count: i64) {
        let mut transaction = self.store.begin().unwrap();
        for n in 0..count {
            let id = RecordId::from(format!("u{n:06}"));
            let at = RecordAddress::new(self.namespace, self.database, self.table, id);
            let mut fields = BTreeMap::new();
            fields.insert("joined".to_owned(), Value::Number(n.into()));
            transaction.put(at, encode_payload(&Value::Object(fields)).into_bytes());
        }
        transaction.commit().unwrap();
    }
}

/// The population every case here reads from: large enough that a linear walk
/// is unmistakable against a bound of ten.
const RECORDS: i64 = 5_000;
/// The bound each descending read asks for.
const WANTED: usize = 10;

// ------------------------------------------------------------------------ C5

/// A bounded descending read examines its bound, not the table.
///
/// The number that would fail this is `RECORDS` — a walk that read every entry
/// and kept ten.
///
/// The ceiling has two terms and both are earned. One
/// `ORDERED_SCAN_BATCH_ENTRIES`: the walk reads in fixed-size batches and
/// cannot know it has enough until it has looked, and it drains the tie group
/// straddling the bound. Plus `WANTED`: each record it keeps is resolved, and a
/// resolution is a one-row scan, so it contributes a row to this count too.
///
/// Written as an expression of the constants rather than as a number, so a
/// fixture that stopped tracking them cannot go on passing while testing
/// nothing.
#[test]
fn a_bounded_descending_read_examines_its_bound_and_not_the_table() {
    let fixture = Fixture::new();
    fixture.write(RECORDS);
    let transaction = fixture.store.begin().unwrap();
    // After `begin`, so the snapshot lookup a transaction does on its way up is
    // not counted as part of what the read cost.
    fixture.counting.reset();
    let found = transaction
        .records_in_descending_order(&fixture.index, 1, WANTED)
        .unwrap()
        .expect("the index holds enough records to fill the bound");
    assert_eq!(found.len(), WANTED);

    let examined = fixture.counting.entries();
    let ceiling = ORDERED_SCAN_BATCH_ENTRIES + WANTED;
    assert!(
        examined <= ceiling,
        "examined {examined} entries for a bound of {WANTED} \
         (ceiling {ceiling}, table {RECORDS})"
    );
    // And it is genuinely a bound rather than an accident of a small table:
    // the table is two orders of magnitude larger than what was read.
    assert!(
        examined * 10 < usize::try_from(RECORDS).unwrap(),
        "examined {examined} of {RECORDS} — that is not a bound"
    );
}

/// The same read, at twice the bound, does not cost twice the table.
///
/// One measurement cannot tell a bound from a constant. This one moves the
/// bound and asserts the cost follows it rather than the table — the shape of
/// the claim, not a single point on it.
#[test]
fn doubling_the_bound_does_not_double_the_table_read() {
    let mut measured = Vec::new();
    for wanted in [WANTED, WANTED * 2] {
        let fixture = Fixture::new();
        fixture.write(RECORDS);
        let transaction = fixture.store.begin().unwrap();
        fixture.counting.reset();
        transaction
            .records_in_descending_order(&fixture.index, 1, wanted)
            .unwrap()
            .expect("the index holds enough");
        measured.push(fixture.counting.entries());
    }
    let ceiling = ORDERED_SCAN_BATCH_ENTRIES + WANTED * 2;
    assert!(
        measured[1] <= ceiling,
        "a bound of {} examined {} entries (ceiling {ceiling})",
        WANTED * 2,
        measured[1]
    );
}

// ------------------------------------------------------------------------ C6

/// What resolving an index range actually costs in round trips.
///
/// **This test records the cost that exists rather than asserting the bound the
/// criterion wants**, and the difference is the finding. G003's C6 claims
/// resolving an index range "costs fewer round trips than it has records". It
/// does not. `records_in_range` reads the entries in batches — that part is
/// bounded — and then resolves **each record with its own one-row scan**, so
/// the round trips are the entry batches plus one per record answered.
///
/// Wave 23 bounded how many index entries the read *holds at once*. That is
/// memory, and it is a different quantity from round trips. Measuring the one
/// the criterion actually named is what separated them.
///
/// Written as an equality rather than an inequality so that the day the
/// resolution is batched, this test **fails** and is rewritten as the bound C6
/// asks for — rather than passing quietly and leaving the criterion looking
/// closed. A test that would not notice the fix is not evidence about the fix.
#[test]
fn resolving_a_range_costs_round_trips_per_batch_and_not_per_record() {
    const IN_RANGE: i64 = 2_000;
    let fixture = Fixture::new();
    fixture.write(IN_RANGE);
    let transaction = fixture.store.begin().unwrap();
    // After `begin`, so the snapshot lookup a transaction does on its way up is
    // not counted as part of what the read cost.
    fixture.counting.reset();
    let found = transaction
        .records_in_range(&fixture.index, &[], None, None)
        .unwrap();
    let records = usize::try_from(IN_RANGE).unwrap();
    assert_eq!(found.len(), records);

    // Two asks per entry batch: one for the entries, one for the records they
    // name. Stated as an equality so that a change in either direction has to
    // be looked at — a `<= records` bound would have passed on the old cost of
    // one round trip per record, which is the cost this test exists to have
    // caught.
    let batches = records.div_ceil(tessari_constants::RANGE_SCAN_BATCH_ENTRIES);
    assert_eq!(
        fixture.counting.round_trips(),
        batches * 2,
        "the whole read should cost two round trips per entry batch ({batches} \
         batches for {records} records)"
    );
    assert!(
        fixture.counting.round_trips() < records,
        "the point of the bound: the cost must not be proportional to the answer"
    );
}

// ------------------------------------------------------------------------ C8

/// What a bounded descending read costs in round trips.
///
/// The entries ceiling above says the walk does not *examine* the table. It says
/// nothing about how many times the backend is asked, and the two are
/// independent: the walk read its entries in bounded batches and then resolved
/// **each record with its own one-row scan**, so the asks were one per record
/// answered — the cost wave 27 removed from `records_in_range` and left here
/// (Q-69), on the grounds that a bound of ten is cheap where a range of two
/// thousand is not.
///
/// Cheap is not free, and it was two call sites resolving records two different
/// ways, which is the shape that drifts.
///
/// Stated as an **equality**, for the reason the range test states it: a
/// `<= WANTED` bound would have passed on the old cost of one round trip per
/// record, which is the cost this test exists to have caught. Two asks: one scan
/// for the entry batch, one batched resolution for the records it named.
#[test]
fn a_bounded_descending_read_costs_two_asks_and_not_one_per_record() {
    let fixture = Fixture::new();
    fixture.write(RECORDS);
    let transaction = fixture.store.begin().unwrap();
    // After `begin`, so the snapshot lookup a transaction does on its way up is
    // not counted as part of what the read cost.
    fixture.counting.reset();
    let found = transaction
        .records_in_descending_order(&fixture.index, 1, WANTED)
        .unwrap()
        .expect("the index holds enough records to fill the bound");
    assert_eq!(found.len(), WANTED);

    assert_eq!(
        fixture.counting.round_trips(),
        2,
        "one ask for the entry batch and one for the records it named, \
         answering with {WANTED} of {RECORDS}"
    );
}

/// Doubling the bound does not double the asks.
///
/// One measurement cannot tell a constant from something proportional to the
/// answer. The entries the walk reads follow the bound; the number of times it
/// asks must not.
#[test]
fn doubling_the_bound_does_not_double_the_asks() {
    let mut measured = Vec::new();
    for wanted in [WANTED, WANTED * 8] {
        let fixture = Fixture::new();
        fixture.write(RECORDS);
        let transaction = fixture.store.begin().unwrap();
        fixture.counting.reset();
        transaction
            .records_in_descending_order(&fixture.index, 1, wanted)
            .unwrap()
            .expect("the index holds enough");
        measured.push(fixture.counting.round_trips());
    }
    assert_eq!(
        measured[0], measured[1],
        "asks moved with the bound: {measured:?}"
    );
}
