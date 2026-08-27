//! A range on a composite index's **second** field, under an equality on its
//! first.
//!
//! `DEFINE INDEX by_at_tag ON events FIELDS at, tag` stores the entries sharing
//! one `at` contiguously and, inside that run, ordered by `tag`. So
//! `at = 20 AND tag >= 1950 AND tag <= 1959` names a *slice* of one run — a
//! bound on a walk the store already knows how to do, not a filter applied over
//! the whole run.
//!
//! Until this wave the range narrowed nothing. `plan::serving` matched an index
//! by `fields.first()`, so a bound on `tag` found a single-field `(tag)` index
//! and never `(at, tag)`; the equality on `at` won the read and every entry in
//! the day's group was fetched, decoded and re-tested above the source.
//!
//! # The obstacle was in the ranking, not the key
//!
//! The key construction generalises in one argument — `IndexValues::leading`
//! already takes a slice of values, and the existing call site passes a
//! one-element one. What actually refused this read was `plan::better`, which
//! compares `Shape` **before** the fixed-field count, and `Shape::Equality`
//! sorts before `Shape::Range`. Shape's own doc gives the reason: *"a single
//! value beats a range, because a range can be the whole table and a value
//! cannot be more than the records holding it."*
//!
//! True of a range on a **leading** field, and false of this one. A range
//! carrying a fixed prefix cannot be larger than the prefix's group, and the
//! equality candidate it competes with **is** that group — so the range is a
//! strict subset of its rival, which is the same proof `Served::fixed` already
//! documents itself as carrying. A proof outranks a preference, so the count
//! moves above the shape.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

/// How many records the table holds.
const RECORDS: i64 = 4_000;

/// How many records share one `at`.
const GROUP: i64 = 100;

/// The group every read below narrows inside.
const AT: i64 = 20;

/// A backend that answers exactly as the one beneath it and counts the rows it
/// handed back.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    rows: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryBackend::new()),
            rows: AtomicUsize::new(0),
        })
    }

    fn reset(&self) {
        self.rows.store(0, AtomicOrdering::Relaxed);
    }

    fn rows(&self) -> usize {
        self.rows.load(AtomicOrdering::Relaxed)
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<KvValue>> {
        let found = self.inner.get(keyspace, key)?;
        if found.is_some() {
            self.rows.fetch_add(1, AtomicOrdering::Relaxed);
        }
        Ok(found)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, KvValue)>> {
        let found = self.inner.scan(request)?;
        self.rows.fetch_add(found.len(), AtomicOrdering::Relaxed);
        Ok(found)
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, KvValue)>>> {
        let found = self.inner.first_of_each(keyspace, ranges)?;
        self.rows
            .fetch_add(found.iter().flatten().count(), AtomicOrdering::Relaxed);
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.inner.apply(batch)
    }
}

/// `RECORDS` events in groups of [`GROUP`] sharing one `at`, whose `tag` runs
/// **opposite** to the record's identity.
///
/// The opposition is inherited from the previous wave's fixture and still earns
/// its keep here: it means the ten records a range answers with are not the ten
/// a prefix of the group would answer with, so a read that fetched the group and
/// filtered afterwards cannot accidentally agree.
fn ready<'a>(store: &'a Store, index: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 1..=RECORDS {
        script.push_str(&format!(
            "CREATE events:{n} = {{ at: {}, tag: {} }};\n",
            n / GROUP,
            RECORDS.saturating_sub(n)
        ));
        if n % 100 == 0 {
            session.run(&script).unwrap();
            script.clear();
        }
    }
    if !script.is_empty() {
        session.run(&script).unwrap();
    }
    if !index.is_empty() {
        session.run(index).unwrap();
    }
    session
}

/// The composite this wave teaches the planner to range inside.
const COMPOSITE: &str = "DEFINE INDEX by_at_tag ON events FIELDS at, tag;";

fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

/// The identities a read answers with, sorted, and the path it took.
///
/// Sorted because a range is not an order and this file asserts nothing about
/// the order — the previous wave's file owns that claim.
fn answered(session: &mut Session<'_>, read: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut ids: Vec<RecordId> = records.iter().map(|(id, _)| id.clone()).collect();
    ids.sort();
    (ids, plan.access)
}

/// A slice of ten tags inside one group, written as two conjuncts.
const SLICE: &str = "SELECT * FROM events WHERE at = 20 AND tag >= 1950 AND tag <= 1959;";

/// The same group with nothing narrowing inside it.
const WHOLE_GROUP: &str = "SELECT * FROM events WHERE at = 20;";

/// The identities `SLICE` must answer with.
///
/// `tag` is `RECORDS - n`, so `tag ∈ [1950, 1959]` is `n ∈ [2041, 2050]`, and
/// every one of those holds `at = 20` because `n / 100 == 20` for `n` in
/// 2000…2099. Computed here from the fixture's own two rules rather than
/// written out, so a change to either is a compile-time change here too.
fn slice_identities() -> Vec<RecordId> {
    let mut ids: Vec<RecordId> = (1..=RECORDS)
        .filter(|n| {
            n / GROUP == AT && {
                let tag = RECORDS.saturating_sub(*n);
                (1950..=1959).contains(&tag)
            }
        })
        .map(RecordId::Int)
        .collect();
    ids.sort();
    ids
}

#[test]
fn a_range_under_an_equality_is_served_by_the_composite_that_holds_both() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);
    assert_eq!(plan(&mut session, SLICE, "access"), r#"String("index")"#);
    assert_eq!(plan(&mut session, SLICE, "index"), r#"String("by_at_tag")"#);
    assert_eq!(plan(&mut session, SLICE, "shape"), r#"String("range")"#);
}

#[test]
fn the_read_costs_the_slice_and_not_the_group() {
    // The assertion the node rests on. A read that took the equality candidate
    // fetches all `GROUP` entries of the day and filters above the source; one
    // that took the range fetches the ten the bounds name. Both answer the same
    // ten records, so only the count can tell them apart.
    let counting = Counting::new();
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);

    counting.reset();
    let (ids, path) = answered(&mut session, SLICE);
    let slice = counting.rows();
    assert_eq!(path, AccessPath::Index);
    assert_eq!(ids, slice_identities());

    counting.reset();
    let (group, group_path) = answered(&mut session, WHOLE_GROUP);
    let whole = counting.rows();
    assert_eq!(group_path, AccessPath::Index);
    assert_eq!(group.len(), usize::try_from(GROUP).unwrap());

    // Deliberately a ratio rather than an exact figure: the read also pays for
    // the catalog and for resolving the records it answers with, and pinning
    // those would make this test fail for reasons that are not this claim.
    // Three is comfortably below `GROUP / 10` and comfortably above one.
    assert!(
        slice * 3 < whole,
        "a slice of ten inside a group of {GROUP} cost {slice} rows against {whole} for the group"
    );
}

#[test]
fn the_answer_is_what_the_scan_answers() {
    let served = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut with = ready(&served, COMPOSITE);
    let scanned = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut without = ready(&scanned, "");

    let (from_index, path) = answered(&mut with, SLICE);
    let (from_scan, scan_path) = answered(&mut without, SLICE);
    assert_eq!(path, AccessPath::Index);
    assert_eq!(scan_path, AccessPath::Scan);
    assert_eq!(from_index, from_scan);
    assert_eq!(from_index, slice_identities());
}

#[test]
fn one_sided_bounds_answer_what_the_scan_answers() {
    // Where a prefix-plus-`after` construction goes wrong: with no lower bound
    // the walk must start at the group, not at the whole index, and with no
    // upper bound it must stop at the group's end, not at the index's.
    let served = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut with = ready(&served, COMPOSITE);
    let scanned = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut without = ready(&scanned, "");

    for read in [
        "SELECT * FROM events WHERE at = 20 AND tag >= 1950;",
        "SELECT * FROM events WHERE at = 20 AND tag <= 1959;",
        "SELECT * FROM events WHERE at = 20 AND tag > 1950 AND tag < 1959;",
    ] {
        let (from_index, path) = answered(&mut with, read);
        let (from_scan, scan_path) = answered(&mut without, read);
        assert_eq!(path, AccessPath::Index, "{read}");
        assert_eq!(scan_path, AccessPath::Scan, "{read}");
        assert_eq!(from_index, from_scan, "{read}");
        assert!(!from_index.is_empty(), "{read}");
    }
}

#[test]
fn a_range_on_a_later_field_with_nothing_fixing_the_first_is_refused() {
    // The read this wave deliberately does not build: without a value for `at`,
    // the tags in range are scattered across every group, and finding them means
    // visiting each group's slice in turn — a different traversal, and one that
    // wants its own node rather than arriving as a side effect of this one.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);
    let read = "SELECT * FROM events WHERE tag >= 1950 AND tag <= 1959;";
    assert_eq!(plan(&mut session, read, "access"), r#"String("scan")"#);
    assert_eq!(answered(&mut session, read).1, AccessPath::Scan);
}

#[test]
fn a_range_on_a_leading_field_is_served_exactly_as_before() {
    // The regression proof for the generalisation: with nothing fixed, the key
    // bounds this wave builds are byte-identical to the ones it replaces, so a
    // range on a single-field index must be untouched in path, index and answer.
    let served = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut with = ready(&served, "DEFINE INDEX by_tag ON events FIELDS tag;");
    let scanned = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut without = ready(&scanned, "");

    let read = "SELECT * FROM events WHERE tag >= 1950 AND tag <= 1959;";
    assert_eq!(plan(&mut with, read, "index"), r#"String("by_tag")"#);
    let (from_index, path) = answered(&mut with, read);
    let (from_scan, _) = answered(&mut without, read);
    assert_eq!(path, AccessPath::Index);
    assert_eq!(from_index, from_scan);
}

#[test]
fn a_single_field_index_on_the_ranged_field_still_wins_when_it_exists() {
    // `(tag)` fixes nothing and bounds one field; `(at, tag)` under this
    // condition fixes one and bounds one. The composite is the strict subset, so
    // it wins — and this test exists because the opposite would also look
    // plausible: the single-field index is the shorter entry, which is what
    // decided the previous wave's tie.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);
    session
        .run("DEFINE INDEX by_tag ON events FIELDS tag;")
        .unwrap();
    assert_eq!(plan(&mut session, SLICE, "index"), r#"String("by_at_tag")"#);
}
