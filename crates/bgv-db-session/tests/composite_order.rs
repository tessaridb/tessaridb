//! A bounded order served from a **composite** index on its leading field.
//!
//! `DEFINE INDEX by_at_tag ON events FIELDS at, tag` stores its entries by `at`
//! before anything else, so it already holds the order `ORDER BY at` asks for.
//! Until this wave it was refused, and the reason on record — in `descending.rs`,
//! in `evaluate.rs` and in `docs/bgvql.md` §8 — was that the tie group at the
//! bound is a group of leading values *"which cannot be read off a key, because
//! the encoding normalises and is not reversible"*.
//!
//! The premise is true. The conclusion does not follow, and that is what this
//! file exists to hold in place: a tie test never asks what an entry **holds**,
//! only whether two entries **agree**, and agreement is byte equality over a
//! self-delimiting prefix. The normalisation that destroys reversibility is what
//! makes byte equality the *right* test rather than a workaround — `1` and `1.0`
//! are one value and belong in one tie group.
//!
//! # Why the tie group is the whole node
//!
//! A single-field key is `value ++ identity`, so a forward walk yields a tie
//! group already ordered by identity ascending — the answer's own order. That is
//! why the ascending read built in the previous wave needs no drain at all.
//!
//! **A composite key is `at ++ tag ++ identity`**, so the entries sharing one
//! `at` are ordered by `tag`. A bound cut inside that group takes the ten
//! smallest by `(at, tag, id)` where the answer wants the ten smallest by
//! `(at, id)` — different records, all of them real, nothing raised. So a
//! composite order drains its group in **both** directions, and the fixture
//! below is built so that a walk which failed to drain would answer with the
//! group's identities in reverse.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use bgv_db_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use bgv_db_session::{AccessPath, Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

/// How many records the table holds.
const RECORDS: i64 = 4_000;

/// How many records share one `at`, so a bound of [`LIMIT`] falls **inside** a
/// tie group rather than at its edge.
const GROUP: i64 = 100;

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

/// `RECORDS` events in groups of [`GROUP`] sharing one `at`, whose `tag` runs
/// **opposite** to the record's identity.
///
/// The opposition is the point. Inside a group the composite's entries are
/// ordered by `tag`, so they come out with their identities *descending* — the
/// exact reverse of the order the answer breaks ties in. A walk that cut at the
/// bound instead of draining would therefore answer with the group's ten
/// **largest** identities where the read asks for its ten smallest, and every
/// record it returned would be real.
///
/// `index` is the only thing that differs between a served fixture and its scan.
fn ready<'a>(store: &'a Store, index: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events;\n\
             DEFINE FIELD at ON events TYPE int REQUIRED;",
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

/// The composite whose leading field is the one every read here orders by.
const COMPOSITE: &str = "DEFINE INDEX by_at_tag ON events FIELDS at, tag;";

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
    let Some(Outcome::Records { records, path }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (records.iter().map(|(id, _)| id.clone()).collect(), *path)
}

const ASCENDING: &str = "SELECT * FROM events ORDER BY at LIMIT 10;";
const DESCENDING: &str = "SELECT * FROM events ORDER BY at DESC LIMIT 10;";

#[test]
fn a_composite_index_is_offered_for_an_order_on_its_leading_field() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);
    for read in [ASCENDING, DESCENDING] {
        assert_eq!(plan(&mut session, read, "access"), r#"String("ordered")"#);
        assert_eq!(
            plan(&mut session, read, "index"),
            r#"String("by_at_tag")"#,
            "{read}"
        );
        assert_eq!(
            answered(&mut session, read).1,
            AccessPath::Ordered,
            "{read}"
        );
    }
}

#[test]
fn the_tie_group_at_the_bound_is_drained_and_not_cut() {
    // The assertion the whole node rests on. Inside one `at` the composite's
    // entries run by `tag`, which this fixture makes the reverse of identity —
    // so a walk that stopped at the bound would answer with the group's largest
    // identities. The answer wants its smallest.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);

    let (ids, path) = answered(&mut session, ASCENDING);
    assert_eq!(path, AccessPath::Ordered);
    // `at` is `n / 100`, so `events:1` … `events:99` all hold zero and are the
    // least group. Ties break by identity ascending, so the answer is the first
    // ten identities of that group and not its last ten.
    let expected: Vec<RecordId> = (1..=10).map(RecordId::Int).collect();
    assert_eq!(ids, expected);
}

#[test]
fn both_directions_answer_what_the_scan_answers_in_the_same_order() {
    let served = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut with = ready(&served, COMPOSITE);
    let scanned = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut without = ready(&scanned, "");

    for read in [ASCENDING, DESCENDING] {
        let (from_index, path) = answered(&mut with, read);
        let (from_scan, scan_path) = answered(&mut without, read);
        assert_eq!(path, AccessPath::Ordered, "{read}");
        assert_eq!(scan_path, AccessPath::Scan, "{read}");
        // Record for record **and in answer order**: the order is what three
        // waves of this goal changed, and a comparison that sorted first could
        // not see it.
        assert_eq!(from_index, from_scan, "{read}");
        assert_eq!(from_index.len(), LIMIT, "{read}");
    }
}

#[test]
fn a_served_composite_order_reads_its_group_and_not_the_table() {
    let counting = Counting::new();
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);

    counting.reset();
    let (ids, path) = answered(&mut session, ASCENDING);
    let served = counting.rows();
    assert_eq!(path, AccessPath::Ordered);
    assert_eq!(ids.len(), LIMIT);

    counting.reset();
    session.run("SELECT * FROM events;").unwrap();
    let whole = counting.rows();

    // A drained group is not free — it reads the whole group rather than the
    // bound — so the claim here is deliberately weaker than the single-field
    // one: far under the table, not near the bound. A read that quietly fell
    // back to the scan would land at `whole`.
    assert!(
        served * 4 < whole,
        "a served composite order cost {served} rows against {whole} for the whole table"
    );
}

#[test]
fn an_order_on_a_later_field_of_the_composite_is_refused() {
    // `tag`'s entries are grouped inside each `at`, so reading them in key order
    // yields `tag` restarted once per group — not that field's order at any
    // point, and not an approximation of it either.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);
    for read in [
        "SELECT * FROM events ORDER BY tag LIMIT 10;",
        "SELECT * FROM events ORDER BY tag DESC LIMIT 10;",
    ] {
        assert_eq!(
            plan(&mut session, read, "access"),
            r#"String("scan")"#,
            "{read}"
        );
        assert_eq!(answered(&mut session, read).1, AccessPath::Scan, "{read}");
    }
}

#[test]
fn a_single_field_index_is_preferred_to_a_composite_holding_the_same_order() {
    // Both hold the order. The shorter entry is fewer bytes, and preferring it
    // keeps every read this store already served on the path it already took —
    // which is why widening the candidate set had to be additive rather than a
    // change of ranking.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = ready(&store, COMPOSITE);
    session
        .run("DEFINE INDEX by_at ON events FIELDS at;")
        .unwrap();
    assert_eq!(plan(&mut session, ASCENDING, "index"), r#"String("by_at")"#);
    assert_eq!(
        plan(&mut session, DESCENDING, "index"),
        r#"String("by_at")"#
    );
}

#[test]
fn an_ascending_composite_order_is_still_refused_over_an_optional_leading_field() {
    // The previous wave's admission rule, inherited unchanged: a record with no
    // `at` has no entry in any index leading with `at`, and ascending those
    // records come first. Being composite changes nothing about that.
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events;\n\
             DEFINE FIELD at ON events TYPE int;\n\
             CREATE events:1 = { at: 5, tag: 1 };\n\
             CREATE events:2 = { tag: 2 };\n\
             DEFINE INDEX by_at_tag ON events FIELDS at, tag;",
        )
        .unwrap();
    let read = "SELECT * FROM events ORDER BY at LIMIT 2;";
    assert_eq!(plan(&mut session, read, "access"), r#"String("scan")"#);
    let (ids, path) = answered(&mut session, read);
    assert_eq!(path, AccessPath::Scan);
    // And the record with no `at` is the one an ascending answer needs first,
    // which is the whole reason for the refusal.
    assert_eq!(ids, vec![RecordId::Int(2), RecordId::Int(1)]);

    // Descending is admitted, because there the absences come last.
    let down = "SELECT * FROM events ORDER BY at DESC LIMIT 1;";
    assert_eq!(plan(&mut session, down, "access"), r#"String("ordered")"#);
}
