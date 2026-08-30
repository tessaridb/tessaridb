//! A bounded **ascending** order served from the index that holds it.
//!
//! Descending has been served from the index since G003. Ascending was refused,
//! and `plan::ordered`'s doc comment said exactly why: the value system puts
//! `none` below every value, a record with no value has **no index entry**, and
//! so ascending the records the index does not hold are precisely the ones the
//! answer needs first. A walk over the index would answer with the wrong records
//! and raise nothing.
//!
//! # The door, and why it is a real one
//!
//! The same comment named the exception — *a `REQUIRED` field, where there are
//! no absences* — and this file is the check that the exception holds rather
//! than sounds plausible. `REQUIRED` is enforced on every write, which alone
//! would not be enough: a record written **before** the declaration would be
//! invisible to it, and the read would answer short.
//!
//! It is enough because the declaration is refused too:
//! `a_required_field_cannot_be_declared_over_a_table_that_already_breaks_it`
//! pins that, because it is the half of the invariant nothing else in this
//! store depends on and therefore the half that could be relaxed by someone who
//! did not know this read was resting on it.
//!
//! # What is asserted in both directions
//!
//! One fixture, two declarations differing **only** in the word `REQUIRED`. The
//! read is served from the index in the first and falls back to the scan in the
//! second, and both answer the same records in the same order. Asserting only
//! the served half would pass a store that served every ascending read,
//! including the ones it must not.

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
///
/// Two orders of magnitude above what a served read examines, for the reason
/// `ordered_under_a_where.rs` records: a fixture too small to tell a bound from
/// a scan cannot support a claim about cost.
const RECORDS: i64 = 4_000;

/// The bound every read here asks for.
const LIMIT: usize = 10;

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

/// `RECORDS` events whose `at` carries **ties in pairs**, so a tie group
/// straddles the bound and the identity tie-break is exercised rather than
/// assumed.
///
/// `declaration` is the only thing that differs between the two halves of every
/// test below.
fn ready<'a>(store: &'a Store, declaration: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events SCHEMALESS;\n\
             {declaration}"
        ))
        .unwrap();
    let mut script = String::new();
    for n in 1..=RECORDS {
        script.push_str(&format!("CREATE events:{n} = {{ at: {} }};\n", n / 2));
        if n % 100 == 0 {
            session.run(&script).unwrap();
            script.clear();
        }
    }
    if !script.is_empty() {
        session.run(&script).unwrap();
    }
    session
        .run("DEFINE INDEX by_at ON events FIELDS at;")
        .unwrap();
    session
}

/// The declaration that makes an ascending read servable.
const REQUIRED: &str = "DEFINE FIELD at ON events TYPE int REQUIRED;";

/// The same declaration without the promise.
const OPTIONAL: &str = "DEFINE FIELD at ON events TYPE int;";

fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

/// The identities a read answers with, **in the order it answered**, and the
/// path it took.
fn answered(session: &mut Session<'_>, read: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        plan.access,
    )
}

/// Every shape this file asserts over, so the two halves cannot drift apart.
///
/// A window and a bound smaller and larger than the tie group, because the tie
/// group is where an ascending walk could take the wrong members and still
/// return real records.
fn shapes() -> Vec<String> {
    vec![
        format!("SELECT * FROM events ORDER BY at LIMIT {LIMIT};"),
        "SELECT * FROM events ORDER BY at LIMIT 1;".to_owned(),
        "SELECT * FROM events ORDER BY at START 3 LIMIT 5;".to_owned(),
        "SELECT * FROM events ORDER BY at LIMIT 137;".to_owned(),
        format!("SELECT * FROM events ORDER BY at LIMIT {};", RECORDS + 10),
    ]
}

#[test]
fn a_bounded_ascending_read_is_taken_from_the_index_that_holds_the_order() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, REQUIRED);
    let read = format!("SELECT * FROM events ORDER BY at LIMIT {LIMIT};");
    assert_eq!(plan(&mut session, &read, "access"), "String(\"ordered\")");
    assert_eq!(plan(&mut session, &read, "index"), "String(\"by_at\")");
    let (_, path) = answered(&mut session, &read);
    assert_eq!(path, AccessPath::Ordered, "the index was not used");
}

#[test]
fn the_same_read_over_an_optional_field_is_refused_and_scans() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, OPTIONAL);
    let read = format!("SELECT * FROM events ORDER BY at LIMIT {LIMIT};");
    // The index exists and holds the order. What it does not hold is a promise
    // that it holds *every record*, and that is the whole difference.
    assert_eq!(plan(&mut session, &read, "access"), "String(\"scan\")");
    let (_, path) = answered(&mut session, &read);
    assert_eq!(path, AccessPath::Scan);
}

#[test]
fn both_answer_the_same_records_in_the_same_order() {
    let served = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let refused = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut with = ready(&served, REQUIRED);
    let mut without = ready(&refused, OPTIONAL);
    for read in shapes() {
        let (from_index, served_by) = answered(&mut with, &read);
        let (from_scan, scanned_by) = answered(&mut without, &read);
        assert_eq!(
            from_index, from_scan,
            "`{read}` answered differently when it was served from the index"
        );
        // The paths asserted **different**, so the equality above is not two
        // scans agreeing with each other — the check G003 had to add after a
        // matrix passed straight through a falsification.
        assert_eq!(scanned_by, AccessPath::Scan, "`{read}`");
        // Every shape, including the bound above the table: what an ascending
        // walk runs out of is the *table*, not the answer, so a short walk is
        // still a served one. Descending would hand that case back to the scan.
        assert_eq!(served_by, AccessPath::Ordered, "`{read}`");
    }
}

#[test]
fn the_first_record_of_an_ascending_answer_is_the_least_and_not_the_greatest() {
    // The assertion an equality against the scan cannot make on its own: if the
    // walk ran the wrong way and the scan's sort were also reversed, the two
    // would agree. This names the value.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, REQUIRED);
    let (ids, path) = answered(
        &mut session,
        &format!("SELECT * FROM events ORDER BY at LIMIT {LIMIT};"),
    );
    assert_eq!(path, AccessPath::Ordered);
    // `at` is `n / 2`, so `events:1` and `events:2` both hold zero and the least
    // value's tie group breaks by identity ascending.
    assert_eq!(ids[0], RecordId::Int(1));
    assert_eq!(ids[1], RecordId::Int(2));
    assert_eq!(ids[2], RecordId::Int(3));
}

#[test]
fn a_bounded_ascending_read_examines_its_bound_and_not_the_table() {
    let counting = Counting::new();
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, REQUIRED);
    let read = format!("SELECT * FROM events ORDER BY at LIMIT {LIMIT};");
    counting.reset();
    let (ids, path) = answered(&mut session, &read);
    let served = counting.rows();
    assert_eq!(path, AccessPath::Ordered);
    assert_eq!(ids.len(), LIMIT);

    counting.reset();
    session.run("SELECT * FROM events;").unwrap();
    let whole = counting.rows();

    // The number that fails this is the table's. A read that quietly fell back
    // to the scan would land at `whole` and this is what says so.
    assert!(
        served * 4 < whole,
        "a bounded ascending read cost {served} rows against {whole} for the whole table"
    );
}

#[test]
fn a_route_below_a_required_field_is_refused() {
    // `REQUIRED` is declared on a field, so it promises nothing about what lives
    // inside one: a required `address` does not promise an `address.city`. The
    // read is refused rather than approximated.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE people SCHEMALESS;\n\
             DEFINE FIELD address ON people TYPE object REQUIRED;\n\
             CREATE people:1 = { address: { city: 'london' } };\n\
             CREATE people:2 = { address: { } };\n\
             DEFINE INDEX by_city ON people FIELDS address.city;",
        )
        .unwrap();
    let read = "SELECT * FROM people ORDER BY address.city LIMIT 2;";
    assert_eq!(plan(&mut session, read, "access"), "String(\"scan\")");
    let (ids, path) = answered(&mut session, read);
    assert_eq!(path, AccessPath::Scan);
    // And the record the index does not hold is the one an ascending answer
    // needs first, which is the whole reason for the refusal.
    assert_eq!(ids, vec![RecordId::Int(2), RecordId::Int(1)]);
}

#[test]
fn a_required_field_cannot_be_declared_over_a_table_that_already_breaks_it() {
    // The half of the invariant nothing else depends on, pinned here because the
    // ascending read rests on it: enforcement on every write would not be enough
    // if a record written before the declaration could survive it.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION events;\n\
             CREATE events:1 = { at: 1 };\n\
             CREATE events:2 = { };",
        )
        .unwrap();
    let refused = session.run(REQUIRED);
    assert!(
        refused.is_err(),
        "a table already holding a record without the field accepted the declaration"
    );
}

#[test]
fn a_descending_read_over_an_optional_field_is_still_served() {
    // The direction rule must not have become a `REQUIRED` rule: descending has
    // no absence problem, because absences sort last and a bounded read never
    // reaches them.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, OPTIONAL);
    let read = format!("SELECT * FROM events ORDER BY at DESC LIMIT {LIMIT};");
    assert_eq!(plan(&mut session, &read, "access"), "String(\"ordered\")");
    let (_, path) = answered(&mut session, &read);
    assert_eq!(path, AccessPath::Ordered);
}
