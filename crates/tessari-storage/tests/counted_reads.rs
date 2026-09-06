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
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_constants::{ORDERED_SCAN_BATCH_ENTRIES, RANGE_SCAN_BATCH_ENTRIES};
use tessari_encoding::Direction;
use tessari_encoding::encode_payload;
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_storage::{
    Catalog, EDGE_IN, EDGE_OUT, EdgeKindDefinition, FieldShape, IndexDefinition, IndexShape,
    RecordAddress, Store, TableShape,
};
use tessari_types::{
    Analyzer, DatabaseId, FieldKind, Filter, NamespaceId, Path, RecordId, RecordRef, TableId, Value,
};

/// A backend that answers exactly as the one beneath it and says what it was
/// asked.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    entries: AtomicUsize,
    scans: AtomicUsize,
    point_reads: AtomicUsize,
    largest: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryBackend::new()),
            entries: AtomicUsize::new(0),
            scans: AtomicUsize::new(0),
            point_reads: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
        })
    }

    /// Start counting from here, so a fixture's own writes are not the answer.
    fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
        self.scans.store(0, Ordering::Relaxed);
        self.point_reads.store(0, Ordering::Relaxed);
        self.largest.store(0, Ordering::Relaxed);
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    /// The most pairs any single fetch handed back.
    ///
    /// A separate quantity from [`Self::entries`], and the distinction is the
    /// whole point of a batched walk: reading a range of ten thousand entries in
    /// batches still *examines* ten thousand, and what changes is that it never
    /// holds more than one batch of them. A cumulative count cannot tell those
    /// two apart, so it cannot fail when the bound is removed.
    fn largest_fetch(&self) -> usize {
        self.largest.load(Ordering::Relaxed)
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
        self.largest.fetch_max(found.len(), Ordering::Relaxed);
        Ok(found)
    }

    /// Delegated, for the same reason [`Self::first_of_each`] is.
    ///
    /// The trait's default answers a count by scanning, so taking it here would
    /// have this wrapper report entries for a call that, on both real backends,
    /// returns none — and the test asserting that a count reads no entries would
    /// be asserting it about the wrapper's own default rather than about the
    /// backend beneath it. It would then fail while the code under test was
    /// right, which is the more expensive direction of wrong.
    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        self.inner.count(keyspace, range)
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

    /// `count` records, every one of them holding the same `joined` value.
    ///
    /// The distinct-value fixture above cannot exercise a wide equality lookup:
    /// each value names one entry, so the walk has one entry to walk.
    fn write_sharing(&self, count: usize, joined: i64) {
        let mut transaction = self.store.begin().unwrap();
        for n in 0..count {
            let id = RecordId::from(format!("u{n:06}").as_str());
            let at = RecordAddress::new(self.namespace, self.database, self.table, id);
            let mut fields = BTreeMap::new();
            fields.insert("joined".to_owned(), Value::from(joined));
            transaction.put(at, encode_payload(&Value::Object(fields)).into_bytes());
        }
        transaction.commit().unwrap();
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

// ------------------------------------------------------------------- F-002

/// An equality lookup over many entries never holds more than one batch.
///
/// This is the property the batched walk exists for, and it needs the
/// **largest single fetch** rather than the cumulative count: a walk that reads
/// ten thousand entries one batch at a time still examines ten thousand, so a
/// cumulative assertion here would either be trivially true or fail for the
/// wrong reason. The number that fails this is the entry count itself — one
/// fetch that returned the whole list, which is what this call did before the
/// walk was bounded.
#[test]
fn an_equality_lookup_never_holds_more_than_one_batch_of_entries() {
    let fixture = Fixture::new();
    let sharing = RANGE_SCAN_BATCH_ENTRIES * 4;
    fixture.write_sharing(sharing, 7);
    let transaction = fixture.store.begin().unwrap();
    fixture.counting.reset();
    let found = transaction
        .records_by_index(&fixture.index, &[Value::from(7_i64)])
        .unwrap();
    assert_eq!(found.len(), sharing, "the lookup lost records");

    let largest = fixture.counting.largest_fetch();
    assert!(
        largest <= RANGE_SCAN_BATCH_ENTRIES,
        "one fetch returned {largest} entries of {sharing} \
         (batch {RANGE_SCAN_BATCH_ENTRIES}) — the walk is not bounded"
    );
}

/// A document frequency is answered without reading a single posting.
///
/// The sharpest case of the finding: this used to scan the term's whole posting
/// list — every key **and value** decoded into a `Vec` — so that `.len()` could
/// be read off it, once per query term per query. Asking the backend to count
/// returns no entries at all, which is what makes the assertion below `0`
/// rather than a ceiling.
#[test]
fn a_document_frequency_reads_no_postings_at_all() {
    let fixture = SearchFixture::new();
    let posted = RANGE_SCAN_BATCH_ENTRIES * 4;
    fixture.write(posted);
    let transaction = fixture.store.begin().unwrap();
    fixture.counting.reset();
    let frequency = transaction
        .document_frequency(&fixture.index, "lock")
        .unwrap();
    assert_eq!(
        frequency,
        u64::try_from(posted).unwrap(),
        "the count is wrong, so the cost below measures nothing"
    );
    assert_eq!(
        fixture.counting.entries(),
        0,
        "counting the postings returned entries — it materialised them"
    );
}

/// A store whose index posts terms, for the count above.
struct SearchFixture {
    counting: Arc<Counting>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    index: IndexDefinition,
}

impl SearchFixture {
    fn new() -> Self {
        let counting = Counting::new();
        let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "shop").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "notes", TableShape::default())
            .unwrap();
        // The analyzer is bound to the **field**, not to the index — so a search
        // index over a field that declares none posts no terms at all, silently.
        // The frequency assertion in the test is what catches that, which is why
        // it is there rather than trusted.
        catalog
            .create_analyzer("plain", Analyzer::new(vec![Filter::Lowercase]))
            .unwrap();
        catalog
            .create_field(
                table.id,
                "body",
                FieldKind::String,
                FieldShape {
                    required: false,
                    default: None,
                    analyzer: Some("plain".to_owned()),
                    assert: None,
                },
            )
            .unwrap();
        let index = catalog
            .create_index(
                table.id,
                "by_body",
                vec![Path::field("body")],
                IndexShape {
                    unique: false,
                    search: true,
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

    /// `count` records, every one of them posting the term `lock`.
    fn write(&self, count: usize) {
        let mut transaction = self.store.begin().unwrap();
        for n in 0..count {
            let id = RecordId::from(format!("n{n:06}").as_str());
            let at = RecordAddress::new(self.namespace, self.database, self.table, id);
            let mut fields = BTreeMap::new();
            fields.insert("body".to_owned(), Value::from("lock contention"));
            transaction.put(at, encode_payload(&Value::Object(fields)).into_bytes());
        }
        transaction.commit().unwrap();
    }
}

// ------------------------------------------------------------------------ C3

/// A graph with one edge kind, and a hub whose degree the caller chooses.
///
/// Its own fixture rather than a method on the one above, because that one is
/// built around an ordered index on a table of users and a graph needs a second
/// table, a graph, an edge kind and its companion table. Sharing it would mean
/// every case here paying for a graph it does not read.
struct Hub {
    counting: Arc<Counting>,
    store: Store,
    kind: EdgeKindDefinition,
    people: TableId,
    hub: RecordId,
}

impl Hub {
    /// A hub joined to `degree` other records by edges of one kind.
    fn of_degree(degree: usize) -> Self {
        let counting = Counting::new();
        let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
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
        let kind = catalog
            .create_edge_kind(&graph, "knows", people.id, people.id, edges.id)
            .unwrap();
        transaction.commit().unwrap();

        let hub = RecordId::from("hub");
        let mut transaction = store.begin().unwrap();
        for n in 0..degree {
            let far = RecordId::from(format!("p{n:06}").as_str());
            let mut fields = BTreeMap::new();
            fields.insert(
                EDGE_OUT.to_owned(),
                Value::Record(RecordRef::new(people.id, hub.clone())),
            );
            fields.insert(
                EDGE_IN.to_owned(),
                Value::Record(RecordRef::new(people.id, far)),
            );
            transaction.put(
                RecordAddress::new(
                    namespace.id,
                    database.id,
                    edges.id,
                    RecordId::from(format!("e{n:06}").as_str()),
                ),
                encode_payload(&Value::Object(fields)).into_bytes(),
            );
        }
        transaction.commit().unwrap();

        Self {
            counting,
            store,
            kind,
            people: people.id,
            hub,
        }
    }

    /// One hop out of the hub, counted from a standing start.
    ///
    /// The reset comes **after** `begin`, deliberately. Opening a transaction
    /// reads the state it is a snapshot of, and that ask belongs to the
    /// transaction rather than to the hop — counted here it would be a constant
    /// term added to a figure whose whole claim is what the constant is. The
    /// first draft reset before `begin` and measured two asks for one read.
    fn hop(&self) -> (usize, usize, usize) {
        let transaction = self.store.begin().unwrap();
        self.counting.reset();
        let found = transaction
            .neighbours(&self.kind, self.people, &self.hub, Direction::Out)
            .unwrap();
        (
            found.len(),
            self.counting.round_trips(),
            self.counting.entries(),
        )
    }
}

/// A hop is one range read, and stays one however wide the node is.
///
/// **This is C3, and it is a claim about the number of asks — never about the
/// number of rows.** The answer holds one member per neighbour under every
/// possible layout, so rows are proportional to degree and always will be. What
/// separates the adjacency layout from the index probe it replaced is that the
/// probe is followed by a read *per neighbour*: `1 + degree` asks against one.
/// At depth three over a fanned-out node that is thousands of random reads.
///
/// Six records make the two indistinguishable, which is why no other test in
/// this suite can make this assertion and why it is made by counting rather
/// than by timing. Timing would turn an exact structural property into a noisy
/// one and would vary with the machine.
///
/// The number that fails this is `1 + degree`.
#[test]
fn a_hop_costs_one_ask_however_many_neighbours_the_node_has() {
    let narrow = Hub::of_degree(4);
    let wide = Hub::of_degree(400);

    let (narrow_found, narrow_asks, narrow_entries) = narrow.hop();
    let (wide_found, wide_asks, wide_entries) = wide.hop();

    assert_eq!(
        narrow_found, 4,
        "the narrow hub answered with the wrong count"
    );
    assert_eq!(
        wide_found, 400,
        "the wide hub answered with the wrong count"
    );

    // The claim. Equal, not merely sub-linear: the layout promises one range
    // read over a contiguous prefix, and one is a number rather than a trend.
    assert_eq!(
        narrow_asks, wide_asks,
        "a hundredfold wider node cost {wide_asks} asks against {narrow_asks}"
    );
    assert_eq!(narrow_asks, 1, "a hop should be exactly one range read");

    // The inversion check, and it is not decoration. Without it the equality
    // above passes when both hubs answer with nothing — two empty reads cost
    // the same and prove no property at all. Rows are what must differ, because
    // rows are the thing that legitimately grows with degree.
    assert!(
        wide_entries > narrow_entries,
        "both hubs read {narrow_entries} entries, so the fixtures do not differ"
    );
    assert_eq!(
        wide_entries, 400,
        "the wide hop read something other than its answer"
    );
}

// ------------------------------------------------------------- the table walk

/// A walk the caller stops examines a batch, not the table.
///
/// This is the whole claim of `walk_table` stated as a cost. A read with a
/// `WHERE` cannot push its `LIMIT` into the source — the bound counts records
/// that match and the source counts records that exist — so what stops it is
/// the caller's `Break`. `scan_table` cannot hear one: it returns a `Vec`, and
/// the number that fails this test is `RECORDS`, which is what a table read
/// whole costs whether the caller wanted ten records or one.
///
/// Counted at the backend rather than by the walk, for the reason this file
/// exists: a walk that reported its own cost would agree with itself.
#[test]
fn a_walk_the_caller_stops_examines_a_batch_and_not_the_table() {
    let fixture = Fixture::new();
    fixture.write(RECORDS);
    let mut transaction = fixture.store.begin().unwrap();
    fixture.counting.reset();

    let mut handed = 0_usize;
    transaction
        .walk_table(
            fixture.namespace,
            fixture.database,
            fixture.table,
            |_, _, _| {
                handed += 1;
                Ok::<_, tessari_storage::Error>(if handed == WANTED {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            },
        )
        .unwrap();
    assert_eq!(handed, WANTED);

    let examined = fixture.counting.entries();
    assert!(
        examined <= RANGE_SCAN_BATCH_ENTRIES,
        "examined {examined} entries to hand over {WANTED} \
         (one batch is {RANGE_SCAN_BATCH_ENTRIES}, table {RECORDS})"
    );
    // And it is a bound rather than a small table: the walk stopped inside the
    // first batch of a table four times that size.
    assert!(
        examined * 4 < usize::try_from(RECORDS).unwrap(),
        "examined {examined} of {RECORDS} — that is not a bound"
    );
}

/// A walk over the whole table still holds one batch at a time.
///
/// The other half, and it is a different quantity: a walk that reads everything
/// legitimately *examines* everything, and what must not grow with the table is
/// how much of it is in hand at once. `scan_table` fails this by construction —
/// it asks the backend for the table in a single request — which is the memory
/// half of the same finding (Q-72).
#[test]
fn a_walk_over_the_whole_table_holds_one_batch_at_a_time() {
    let fixture = Fixture::new();
    fixture.write(RECORDS);
    let mut transaction = fixture.store.begin().unwrap();
    fixture.counting.reset();

    let mut handed = 0_usize;
    transaction
        .walk_table(
            fixture.namespace,
            fixture.database,
            fixture.table,
            |_, _, _| {
                handed += 1;
                Ok::<_, tessari_storage::Error>(ControlFlow::Continue(()))
            },
        )
        .unwrap();
    assert_eq!(handed, usize::try_from(RECORDS).unwrap());
    assert!(
        fixture.counting.largest_fetch() <= RANGE_SCAN_BATCH_ENTRIES,
        "one fetch handed back {} of {RECORDS} records",
        fixture.counting.largest_fetch()
    );
}
