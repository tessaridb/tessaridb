//! An index that would return most of the table loses to reading the table.
//!
//! Until this wave the planner took any applicable index. Ranking answered
//! *which* index narrows most and there was no way to say "an index applied and
//! lost", because the scan was not a candidate and had no number to be compared
//! against — so an ordered index selecting most of a table was chosen while
//! being **slower than no index at all**. Both halves of the read are paid: the
//! entry walk and the record fetch, and neither removes anything.
//!
//! Two numbers were needed and neither alone decides it. A maintained per-table
//! count cannot rank a range, which has no ceiling. A bounded probe of the range
//! means nothing on its own, because a number of entries is only large or small
//! relative to a table size. The rule is a ratio: an index is served when it can
//! produce **at most half** the table.
//!
//! # What this file must never allow itself to become
//!
//! A timing table. Which path runs is a cost decision and a wrong cost raises
//! nothing, so it is asserted by the reported plan; what the read *answers* is
//! asserted separately and against a table carrying no index at all. That
//! second assertion is the one that matters — a planner is the component most
//! tempted to change an answer, and a faster wrong answer is the failure this
//! whole wave could have introduced.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result as KvResult, ScanRequest,
    Value as KvValue, WriteBatch,
};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

/// How many records the table holds.
///
/// Above `PLANNER_SCAN_FLOOR_RECORDS`, because below that floor the guard does
/// not engage at all and every assertion here would pass for the wrong reason.
/// A round four thousand so that "half" is a number the assertions can name
/// rather than one they have to compute around a remainder.
const RECORDS: i64 = 4_000;

/// The store, the indexed table and the mirror that carries no index.
///
/// `mirror` holds the same records under the same field names and is never
/// given an index, so a read against it is the scan's answer to the same
/// question — the control every claim about the indexed table is checked
/// against.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION events;\n\
             DEFINE COLLECTION mirror;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 1..=RECORDS {
        // `most` puts nine records in ten under one value and `half` splits the
        // table evenly, so one equality is worth serving and the other is not.
        let (half, most) = (n % 2, i64::from(n % 10 == 0));
        script.push_str(&format!(
            "CREATE events:{n} = {{ n: {n}, half: {half}, most: {most} }};\n\
             CREATE mirror:{n} = {{ n: {n}, half: {half}, most: {most} }};\n"
        ));
        if n % 100 == 0 {
            session.run(&script).unwrap();
            script.clear();
        }
    }
    if !script.is_empty() {
        session.run(&script).unwrap();
    }
    session
        .run(
            "DEFINE INDEX by_n ON events FIELDS n;\n\
             DEFINE INDEX by_half ON events FIELDS half;\n\
             DEFINE INDEX by_most ON events FIELDS most;",
        )
        .unwrap();
    session
}

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A backend that answers exactly as the one beneath it and counts the rows it
/// handed back.
///
/// The unit a cost claim is made in here. A timing table would measure this
/// machine; rows handed back by the substrate is what the two access paths
/// actually differ in, and it is the same number on every machine and in every
/// build profile.
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

    fn get(&self, keyspace: Keyspace, key: &Key) -> KvResult<Option<KvValue>> {
        let found = self.inner.get(keyspace, key)?;
        if found.is_some() {
            self.rows.fetch_add(1, AtomicOrdering::Relaxed);
        }
        Ok(found)
    }

    fn scan(&self, request: &ScanRequest) -> KvResult<Vec<(Key, KvValue)>> {
        let found = self.inner.scan(request)?;
        self.rows.fetch_add(found.len(), AtomicOrdering::Relaxed);
        Ok(found)
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> KvResult<Vec<Option<(Key, KvValue)>>> {
        let found = self.inner.first_of_each(keyspace, ranges)?;
        self.rows
            .fetch_add(found.iter().flatten().count(), AtomicOrdering::Relaxed);
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> KvResult<()> {
        self.inner.apply(batch)
    }
}

/// The identities a read answers with, sorted, and the path it took.
fn answered(session: &mut Session<'_>, read: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut ids: Vec<RecordId> = records.iter().map(|(id, _)| id.clone()).collect();
    ids.sort();
    (ids, plan.access)
}

/// The path `EXPLAIN` reports for a read, which must be the path it takes.
fn explained(session: &mut Session<'_>, read: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get("access").unwrap_or(&Value::None))
}

/// The same condition against the table that has no index, as the truth.
fn without_an_index(session: &mut Session<'_>, condition: &str) -> Vec<RecordId> {
    let (ids, access) = answered(session, &format!("SELECT * FROM mirror WHERE {condition};"));
    assert_eq!(access, AccessPath::Scan, "the mirror carries no index");
    ids
}

/// Both tables answer the same condition with the same identities.
fn agrees(session: &mut Session<'_>, condition: &str, expected: AccessPath) {
    let truth = without_an_index(session, condition);
    let (found, access) = answered(session, &format!("SELECT * FROM events WHERE {condition};"));
    assert_eq!(
        access, expected,
        "`{condition}` took the wrong path over the indexed table"
    );
    assert_eq!(
        found.len(),
        truth.len(),
        "`{condition}` answered with a different number of records than the scan"
    );
    // The identities differ only in their table, so the numeric part is what
    // the two answers are compared on.
    let of = |ids: &[RecordId]| -> Vec<String> { ids.iter().map(|id| format!("{id}")).collect() };
    assert_eq!(
        of(&found),
        of(&truth),
        "`{condition}` answered differently over the indexed table than over the scan"
    );
}

#[test]
fn a_selective_range_is_still_served_by_the_index() {
    let store = store();
    let mut session = ready(&store);
    agrees(&mut session, "n >= 3991", AccessPath::Index);
}

#[test]
fn a_range_over_most_of_the_table_falls_back_to_the_scan() {
    // The Q-407 case exactly: an applicable, correct, ordered index that costs
    // more than reading the table.
    let store = store();
    let mut session = ready(&store);
    agrees(&mut session, "n >= 401", AccessPath::Scan);
}

#[test]
fn the_threshold_is_half_the_table_and_it_is_inclusive() {
    // Written as two reads one record apart, so the rule this wave introduced
    // is pinned rather than described. `n >= 2001` names two thousand entries
    // of four thousand and is served; `n >= 2000` names two thousand and one
    // and is not.
    let store = store();
    let mut session = ready(&store);
    agrees(&mut session, "n >= 2001", AccessPath::Index);
    agrees(&mut session, "n >= 2000", AccessPath::Scan);
}

#[test]
fn an_equality_is_probed_against_the_table_too() {
    let store = store();
    let mut session = ready(&store);
    // One record in ten holds `most = 1`.
    agrees(&mut session, "most = 1", AccessPath::Index);
    // Nine in ten hold `most = 0`, so the index would fetch nearly everything.
    agrees(&mut session, "most = 0", AccessPath::Scan);
}

#[test]
fn an_equality_splitting_the_table_evenly_sits_on_the_threshold() {
    let store = store();
    let mut session = ready(&store);
    // Exactly half, which the rule serves — the boundary stated once more from
    // the equality side, because the probe reaches it by a different route.
    agrees(&mut session, "half = 0", AccessPath::Index);
}

#[test]
fn explain_reports_the_path_the_read_takes() {
    // An `EXPLAIN` that named an index the read then declined would be
    // describing a plan nothing runs, which is worse than reporting nothing.
    let store = store();
    let mut session = ready(&store);
    for (condition, expected) in [
        ("n >= 3991", AccessPath::Index),
        ("n >= 401", AccessPath::Scan),
        ("most = 0", AccessPath::Scan),
    ] {
        let read = format!("SELECT * FROM events WHERE {condition};");
        let (_, taken) = answered(&mut session, &read);
        assert_eq!(taken, expected);
        assert_eq!(
            explained(&mut session, &read),
            format!("{:?}", Value::from(format!("{expected:?}").to_lowercase())),
            "`EXPLAIN` disagreed with the read for `{condition}`"
        );
    }
}

#[test]
fn the_path_the_guard_keeps_costs_a_fraction_of_the_one_it_declines() {
    // The claim this wave rests on, in the one unit that is a property of the
    // store rather than of this machine: rows the substrate handed back.
    //
    // A selective equality is served by the index and touches its entries plus
    // its records. The same answer without an index is the whole table. If the
    // first were not much smaller, the index would not be worth the maintenance
    // it costs on every write, never mind the choosing.
    let backend = Counting::new();
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store);

    backend.reset();
    let (served, access) = answered(&mut session, "SELECT * FROM events WHERE most = 1;");
    let by_index = backend.rows();
    assert_eq!(access, AccessPath::Index);

    backend.reset();
    let (scanned, access) = answered(&mut session, "SELECT * FROM mirror WHERE most = 1;");
    let by_scan = backend.rows();
    assert_eq!(access, AccessPath::Scan);

    assert_eq!(
        served.len(),
        scanned.len(),
        "the two paths answered with different numbers of records"
    );
    assert!(
        by_index.saturating_mul(2) < by_scan,
        "the index path handed back {by_index} rows against the scan's {by_scan}, \
         which is not the saving that makes it worth choosing"
    );
}

#[test]
fn a_table_below_the_floor_plans_exactly_as_it_did_before() {
    // The guard is off below `PLANNER_SCAN_FLOOR_RECORDS`, and this is what
    // that buys: a small table whose owner declared an index still uses it.
    // Without the floor, `USING INDEX by_n` on a handful of records would
    // report — correctly, and about nothing worth reporting — that the read
    // scanned.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION small;\n\
             CREATE small:1 = { n: 1 };\n\
             CREATE small:2 = { n: 2 };\n\
             CREATE small:3 = { n: 3 };\n\
             DEFINE INDEX by_n ON small FIELDS n;",
        )
        .unwrap();

    // Two of three records, which is well past the half the rule would refuse.
    let (found, access) = answered(&mut session, "SELECT * FROM small WHERE n >= 2;");
    assert_eq!(found.len(), 2);
    assert_eq!(access, AccessPath::Index);

    // And the language's own way of saying so is satisfied, which is the test
    // the conformance corpus writes.
    session
        .run("SELECT * FROM small WHERE n >= 2 USING INDEX by_n;")
        .unwrap();
}

// ── What the guard does not reach (W171, for Q-453) ────────────────────────
//
// Q-453 asks whether the guard can be wrong above the floor, and offers doing
// nothing as one option. That option was a stance nobody had measured. Measuring
// it found something the question did not anticipate: in the ordering shapes
// tried, the guard is not the reason the index goes unused, because it is never
// asked. Candidates are enumerated from the *condition*, so a read with no
// condition offers none and there is nothing for the guard to weigh — and a
// declared ordered index therefore does not serve a bounded `ORDER BY` at all.
//
// That is a bigger gap than the one Q-453 is about, and it is a different one.
// Recorded as Q-489 and pinned here so that closing it fails this test loudly
// rather than passing quietly.

#[test]
fn a_bounded_ordering_does_not_use_a_declared_index_and_pays_the_whole_table() {
    let backend = Counting::new();
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store);

    // Ten records wanted, in the order the index already stores them, from a
    // table with an index on exactly that field and no condition to weigh.
    backend.reset();
    let (ordered, access) = answered(&mut session, "SELECT * FROM events ORDER BY n LIMIT 10;");
    let ordered_rows = backend.rows();
    assert_eq!(ordered.len(), 10);
    assert_eq!(
        access,
        AccessPath::Scan,
        "if this now reports Index the gap Q-489 records has been closed — \
         update the question and this test together"
    );

    // The same ten, reached through the index, when a condition selective enough
    // to survive the guard puts the index on the table in the first place.
    backend.reset();
    let (served, access) = answered(
        &mut session,
        "SELECT * FROM events WHERE n >= 3991 ORDER BY n LIMIT 10;",
    );
    let served_rows = backend.rows();
    assert_eq!(access, AccessPath::Index);
    assert_eq!(served.len(), 10);

    // The cost of the gap, in the one unit that is a property of the store
    // rather than of this machine.
    assert!(
        served_rows.saturating_mul(10) < ordered_rows,
        "the unconditioned ordering handed back {ordered_rows} rows for ten records \
         against {served_rows} through the index — if these are close, the ordering \
         path has learned to use the index and this test is the thing to fix"
    );
}
