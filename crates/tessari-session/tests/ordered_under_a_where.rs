//! A bounded descending order served from an index **under a condition**.
//!
//! `SELECT * FROM events WHERE slot = 0 ORDER BY at DESC LIMIT 10` narrowed by
//! the condition and then sorted everything it found. The unconditioned form of
//! the same read has taken its bound from the index since G003; the condition
//! was the only difference, and it cost a full scan and a sort.
//!
//! # The one thing this file exists to catch
//!
//! An index **narrows** and the condition **decides** — every candidate is
//! re-tested against the whole condition above the source. So a walk that fills
//! the caller's bound with ten *entries* can answer with fewer than ten
//! *records*, because some of them fail that test. Not an error and not a crash:
//! real records, fewer of them, returned confidently.
//!
//! Every selectivity below is chosen so that a walk asking **once** for the
//! bound would answer short. A condition matching one record in eight means the
//! top ten entries hold about one match, so a naive version answers with one
//! record where ten exist — and passes any test that only checks the records it
//! did return are the right ones.
//!
//! # And the case that must give the order up
//!
//! How far past the bound the walk has to go depends on how selective the
//! condition is over the order, which is the distribution statistic this store
//! deliberately does not keep. So there is a ceiling, and past it the read falls
//! back to the scan it would have taken anyway. The ceiling bounds the **cost**
//! and never the answer: the one-in-a-hundred read below is answered by the
//! scan, and it answers exactly what the served reads answer.

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
/// **Two orders of magnitude above what a served read examines**, which the
/// first version of this file was not: at six hundred records a walk that
/// retried four times read more rows than the whole table, and the number said
/// so. That was the fixture rather than the read — what a served order costs is
/// set by the bound and the scan batch, not by the table — but a fixture too
/// small to tell a bound from a scan cannot support the claim this file makes.
const RECORDS: i64 = 4_000;

/// The bound every read here asks for.
const LIMIT: usize = 10;

const INDEX: &str = "DEFINE INDEX by_at ON events FIELDS at;";

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

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// `RECORDS` events, with three conditions of very different selectivity over
/// the same order.
///
/// - `slot` is one in eight — selective enough that the order is worth serving
///   from the index, and sparse enough that a walk of exactly the bound answers
///   short.
/// - `rare` is one in a hundred — past the ceiling, so the read gives the order
///   up and scans, and must answer exactly what the served reads answer.
/// - `at` carries **ties in pairs**, so a tie group straddles the bound and the
///   identity tie-break is exercised rather than assumed.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events;",
        )
        .unwrap();
    // Written a hundred at a time rather than one at a time: four thousand
    // round trips through the parser is the slowest part of this file by far.
    let mut script = String::new();
    for n in 1..=RECORDS {
        script.push_str(&format!(
            "CREATE events:{n} = {{ at: {}, slot: {}, rare: {} }};\n",
            n / 2,
            n % 8,
            n % 100
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
}

fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

/// The identities a read answers with, **in the order it answered**.
///
/// Not sorted by the test: the order is what is under test.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    session
        .run(read)
        .unwrap()
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

fn path(session: &mut Session<'_>, read: &str) -> AccessPath {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { path, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    *path
}

/// Every read this file cares about, at three selectivities and two windows.
const READS: &[&str] = &[
    // One in eight: served from the index, and a walk of exactly the bound would
    // answer with about one record instead of ten.
    "SELECT * FROM events WHERE slot = 0 ORDER BY at DESC LIMIT 10;",
    // A window, so `START` is part of the bound rather than applied to it.
    "SELECT * FROM events WHERE slot = 0 ORDER BY at DESC START 5 LIMIT 10;",
    // One in two: comfortably served.
    "SELECT * FROM events WHERE slot < 4 ORDER BY at DESC LIMIT 10;",
    // One in a hundred: past the ceiling, so the order is given up.
    "SELECT * FROM events WHERE rare = 0 ORDER BY at DESC LIMIT 10;",
    // Fewer matches than the bound, and all of them at the *top* of the order —
    // so the walk finds nine, can never find a tenth, and must neither loop nor
    // answer with eight.
    "SELECT * FROM events WHERE at > 1995 ORDER BY at DESC LIMIT 10;",
    // Nothing matches at all.
    "SELECT * FROM events WHERE slot = 99 ORDER BY at DESC LIMIT 10;",
    // A condition the index knows nothing about, over the order it does hold.
    "SELECT * FROM events WHERE slot != 3 ORDER BY at DESC LIMIT 10;",
];

#[test]
fn the_answers_are_the_ones_a_scan_gives() {
    // The rule the whole node could have broken, and the one that would break
    // **quietly**: a short answer is real records, fewer of them, returned with
    // nothing raised. Asserted in answer order, because the order is half of
    // what is under test.
    let plain = store();
    let mut without = ready(&plain);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();

    for read in READS {
        assert_eq!(ids(&mut with, read), ids(&mut without, read), "{read}");
    }
    // …and the counts are written out, so the equality above is not two short
    // answers agreeing with each other.
    assert_eq!(ids(&mut with, READS[0]).len(), LIMIT);
    assert_eq!(ids(&mut with, READS[1]).len(), LIMIT);
    assert_eq!(ids(&mut with, READS[2]).len(), LIMIT);
    assert_eq!(ids(&mut with, READS[3]).len(), LIMIT);
    // Nine matches and a bound of ten: the answer is nine, from whichever path
    // ran. A short answer and a correct one are the same length here, which is
    // why the comparison against the scan above is what carries this row.
    assert_eq!(ids(&mut with, READS[4]).len(), 9);
    assert_eq!(ids(&mut with, READS[5]).len(), 0);
}

#[test]
fn a_selective_condition_takes_the_order_from_the_index() {
    let indexed = store();
    let mut with = ready(&indexed);
    assert_eq!(path(&mut with, READS[0]), AccessPath::Scan);
    with.run(INDEX).unwrap();
    assert_eq!(path(&mut with, READS[0]), AccessPath::Ordered);
    assert_eq!(plan(&mut with, READS[0], "access"), r#"String("ordered")"#);
    assert_eq!(plan(&mut with, READS[0], "index"), r#"String("by_at")"#);
}

#[test]
fn a_condition_too_thin_for_the_order_gives_it_up_and_scans() {
    // The ceiling, observed rather than asserted from the constant: one match in
    // a hundred means filling a bound of ten needs about a thousand entries, and
    // the read stops walking and takes the scan. It still answers exactly what
    // the served reads answer — `the_answers_are_the_ones_a_scan_gives` covers
    // that — so this is a statement about cost.
    let counting = Counting::new();
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    let mut with = ready(&store);
    with.run(INDEX).unwrap();
    assert_eq!(path(&mut with, READS[3]), AccessPath::Scan);
    // …while the plan still reports the path the planner chose, which is the
    // same imprecision the unconditioned case has and for the same reason:
    // whether a walk fills its bound is the read's own question.
    assert_eq!(plan(&mut with, READS[3], "access"), r#"String("ordered")"#);

    // And giving up is **bounded**. Without a ceiling the walk would double all
    // the way to the size of the index before finding out it cannot fill the
    // bound, and every doubling restarts from the top — so the read that
    // eventually scans would first have read the index several times over. The
    // number that would fail this is the one a ceiling-less version produces.
    counting.reset();
    ids(&mut with, READS[3]);
    let thin = counting.rows();
    counting.reset();
    ids(&mut with, "SELECT * FROM events WHERE rare = 0 LIMIT 10;");
    let plain = counting.rows();
    assert!(
        thin < plain.saturating_mul(2),
        "giving the order up cost {thin} rows against {plain} for the same \
         condition with no order at all"
    );
}

#[test]
fn a_served_order_does_not_read_the_whole_table() {
    // The saving, counted at the backend rather than timed. The number that
    // would fail this is `RECORDS`: a read that scanned everything and sorted it.
    let counting = Counting::new();
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store);
    session.run(INDEX).unwrap();

    counting.reset();
    let answered = ids(&mut session, READS[0]);
    let served = counting.rows();
    assert_eq!(answered.len(), LIMIT);

    counting.reset();
    ids(&mut session, "SELECT * FROM events ORDER BY at DESC;");
    let whole = counting.rows();

    assert!(
        served * 4 < whole,
        "served {served} rows against {whole} for the whole table"
    );
    assert!(
        served < usize::try_from(RECORDS).unwrap(),
        "served {served} rows of {RECORDS} — that is not a bound"
    );
}

#[test]
fn every_shape_the_order_cannot_be_taken_from_the_index_still_scans() {
    // Each is a way the order an index holds could differ from the order the
    // read must answer in, and each is refused by name rather than guessed at —
    // the same list the unconditioned case refuses, inherited rather than
    // restated, plus the answers to prove the refusal costs nothing.
    let refused = [
        // Ascending: the records with no value sort first, and those are exactly
        // the ones the index does not hold.
        "SELECT * FROM events WHERE slot = 0 ORDER BY at LIMIT 10;",
        // No bound: the read wants every record, so there is nothing to stop.
        "SELECT * FROM events WHERE slot = 0 ORDER BY at DESC;",
        // A grouping folds the records the order would have chosen between.
        "SELECT slot, count(*) AS n FROM events WHERE slot = 0 \
         GROUP BY slot ORDER BY slot DESC LIMIT 10;",
        // The sort runs after the projection and may name what it produced.
        "SELECT at AS when FROM events WHERE slot = 0 ORDER BY when DESC LIMIT 10;",
        // A computed key is not what any index holds.
        "SELECT * FROM events WHERE slot = 0 ORDER BY at + 1 DESC LIMIT 10;",
        // A field no index holds at all.
        "SELECT * FROM events WHERE slot = 0 ORDER BY rare DESC LIMIT 10;",
    ];
    let plain = store();
    let mut without = ready(&plain);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();

    for read in refused {
        assert_ne!(path(&mut with, read), AccessPath::Ordered, "{read}");
        assert_eq!(ids(&mut with, read), ids(&mut without, read), "{read}");
    }
}

#[test]
fn a_write_in_the_same_transaction_gives_the_order_up() {
    // An uncommitted record has no index entry, because entries are derived at
    // commit — so an order served from the index would place it nowhere. The
    // rule is inherited from the unconditioned case and it has to hold here too,
    // because this path reaches the same walk by a different route.
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();
    let outcomes = with
        .run(&format!(
            "BEGIN; CREATE events:{} = {{ at: 99999, slot: 0, rare: 0 }}; {} COMMIT;",
            RECORDS + 1,
            READS[0]
        ))
        .unwrap();
    let Some(Outcome::Records { records, path }) = outcomes
        .iter()
        .find(|outcome| matches!(outcome, Outcome::Records { .. }))
    else {
        panic!("no read in {outcomes:?}");
    };
    assert_eq!(*path, AccessPath::Scan);
    // …and the record written in this transaction is the first one, which is the
    // answer the scan gives and the index could not have.
    assert_eq!(records.len(), LIMIT);
    assert_eq!(records[0].0, RecordId::Int(RECORDS + 1));
}
