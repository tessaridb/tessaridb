//! A `LIMIT` that reaches the **index-served** source, and the shapes where it
//! must not.
//!
//! `bounded_reads.rs` covers the scan. This file covers the other access path,
//! which had the same fault for the same reason and did not get the same fix:
//! `Session::candidates` hands back a `Vec` of every candidate record, so the
//! caller's loop cannot break until the whole set exists. A bound therefore
//! bought nothing — measured on 100 000 records, `WHERE n > 0 LIMIT 1` cost
//! **144.58 ms** against **168.77 ms** for the same read with no bound at all,
//! while the scan answering the same shape of question cost **0.86 ms** (Q-408).
//!
//! # Half of that read can stop, and half of it provably cannot
//!
//! An index read examines two things: the entries naming the candidates, and
//! the records themselves. Only the second can be bounded, and the reason is
//! the answer rather than the implementation.
//!
//! A bounded index-served read answers **the records a scan of the same
//! predicate answers** — identity order, which `by_rank` below pins by being
//! stored in the reverse of it. The lowest identity among the candidates is not
//! known until every candidate has been named, so the entry walk runs to the
//! end by construction; a walk that stopped early would answer with whichever
//! records the index reached first, which is a different set. What the bound
//! does reach is the fetch, the half whose cost grows with the answer.
//!
//! So the floor here is deliberate and asserted as a **correctness** property:
//! a bounded read that examined fewer entries than the table holds would be
//! faster and wrong.
//!
//! # Why entries and not milliseconds
//!
//! The counting backend reports the rows the source handed back. A read that
//! answers the right ten records having examined all four hundred passes every
//! other test in this crate — the cost is invisible to all of them, and a timing
//! on a table this size would report it as noise. Entries examined is the
//! quantity the promise is about.
//!
//! # The half that matters more than the optimisation
//!
//! An index **narrows** and the condition **decides**: every candidate is
//! re-tested against the whole condition above the source, because a field this
//! session may not read resolves to `NONE` and the transaction's own writes have
//! to be seen. So a source that stops early can stop one record short, and the
//! failure is a **quietly short answer** — not an error, not a crash, just fewer
//! records than the caller asked about, returned confidently.
//!
//! Two things guard against that here. The bound is admitted by the same
//! whitelist the scan half uses, so an `ORDER BY` takes it away rather than
//! answering short; and every bounded answer is compared **record for record**
//! against the same predicate answered with no index at all. The comparison is
//! the acceptance. The counts only say it was not bought by reading everything.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

/// How many records each table holds.
///
/// Two orders of magnitude above the bound, so a read that stopped at the bound
/// and one that read the table cannot be confused by a fencepost.
const RECORDS: u64 = 400;

/// The bound every limited read in this file asks for.
const LIMIT: usize = 10;

/// A backend that answers exactly as the one beneath it and counts the rows it
/// handed back.
///
/// Rows rather than calls: the index path does a range walk and then a point get
/// per candidate, so a call count would report a walk of four hundred entries
/// and a walk of ten as the same single scan.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    entries: AtomicUsize,
}

impl Counting {
    fn new(inner: Arc<dyn KvBackend>) -> Self {
        Self {
            inner,
            entries: AtomicUsize::new(0),
        }
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<KvValue>> {
        let found = self.inner.get(keyspace, key)?;
        if found.is_some() {
            self.entries.fetch_add(1, Ordering::Relaxed);
        }
        Ok(found)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, KvValue)>> {
        let found = self.inner.scan(request)?;
        self.entries.fetch_add(found.len(), Ordering::Relaxed);
        Ok(found)
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, KvValue)>>> {
        let found = self.inner.first_of_each(keyspace, ranges)?;
        self.entries
            .fetch_add(found.iter().flatten().count(), Ordering::Relaxed);
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.inner.apply(batch)
    }
}

/// A store over a counting backend, and the counter beside it.
fn counted() -> (Store, Arc<Counting>) {
    let inner = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let counting = Arc::new(Counting::new(inner));
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    (store, counting)
}

/// Two tables holding the same records: one carrying an ordered index on `n`,
/// one carrying none.
///
/// The mirror is what makes the answers comparable. Timing or counting an
/// index-served read against a read of a *different* shape is the mistake Q-401
/// was retracted for, so the unindexed table answers the **same predicate** over
/// the **same records** and the only difference between them is the index.
///
/// `rank` counts down while the identity counts up, so identity order and value
/// order are different orders — otherwise a bound taken from the source's first
/// ten would accidentally be right where it must not be.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION spans;\n\
             DEFINE COLLECTION mirror;\n\
             DEFINE INDEX by_n ON spans FIELDS n;\n\
             DEFINE INDEX by_rank ON spans FIELDS rank;",
        )
        .unwrap();
    for n in 1..=RECORDS {
        let record = format!(
            "{{ n: {n}, rank: {}, city: 'city {}' }}",
            RECORDS.saturating_sub(n),
            n % 4
        );
        session
            .run(&format!("CREATE spans:{n} = {record};"))
            .unwrap();
        session
            .run(&format!("CREATE mirror:{n} = {record};"))
            .unwrap();
    }
    session
}

/// How the planner reached the records, so a test cannot pass by quietly taking
/// the scan it was written to compare against.
fn path(session: &mut Session<'_>, read: &str) -> AccessPath {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    plan.access
}

/// The whole answer, so an equality is record for record and not identity for
/// identity.
fn answer(session: &mut Session<'_>, read: &str) -> Vec<(RecordId, Value)> {
    session
        .run(read)
        .unwrap()
        .last()
        .unwrap()
        .records()
        .unwrap()
        .to_vec()
}

/// The identities alone, for the places where only the count and the order are
/// under test.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    answer(session, read)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// What a read examined at the backend, with the counter reset first.
fn examined(session: &mut Session<'_>, counting: &Counting, read: &str) -> usize {
    counting.reset();
    let _ = answer(session, read);
    counting.entries()
}

#[test]
fn a_bounded_index_read_stops_where_its_answer_fills() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    // The read is index-served. Asserted rather than assumed: were the planner
    // to take the scan here, the comparison below would be a scan against a
    // scan and would pass while proving nothing.
    //
    // `Index` and not `Ordered` — the latter is a read whose *order* the index
    // serves, which is the shape this file's last test exercises for the
    // opposite reason.
    assert_eq!(
        path(&mut session, "SELECT * FROM spans WHERE n > 0 LIMIT 10;"),
        AccessPath::Index,
    );

    let whole = examined(&mut session, &counting, "SELECT * FROM spans WHERE n > 0;");
    let bounded = examined(
        &mut session,
        &counting,
        "SELECT * FROM spans WHERE n > 0 LIMIT 10;",
    );
    let one = examined(
        &mut session,
        &counting,
        "SELECT * FROM spans WHERE n > 0 LIMIT 1;",
    );

    // An index read examines two things and the bound reaches only one of them,
    // so the two halves are asserted separately rather than as one ratio.
    //
    // The **floor** first, because it is a correctness property and not a cost
    // one: the entry walk runs to the end. A bounded read that examined fewer
    // entries than the table holds would have stopped naming candidates before
    // knowing which of them held the lowest identities, and would answer with
    // whichever the index reached first — a different answer from the one the
    // scan gives, which is the invariant an index exists under.
    // `try_from` rather than `as`: a cast that truncates is exactly the class of
    // silent fault the rest of this file exists to catch.
    let records = usize::try_from(RECORDS).unwrap();
    assert!(
        bounded >= records,
        "a bound of {LIMIT} examined {bounded} entries over {RECORDS} records — \
         the entry walk must not stop early or the answer changes"
    );
    // Then the saving, which is the record half. The number that would fail it
    // is the one the materialised version produced: `bounded` equal to `whole`,
    // because the bound was applied to the answer after every candidate record
    // had already been read.
    let saved = whole.saturating_sub(bounded);
    assert!(
        saved * 4 > records * 3,
        "a bound of {LIMIT} examined {bounded} entries against {whole} unbounded, \
         saving {saved} of the {RECORDS} record reads it did not need"
    );
    assert!(
        one < bounded,
        "a bound of 1 examined {one} entries and a bound of {LIMIT} examined \
         {bounded} — a smaller answer must not cost more"
    );
}

#[test]
fn the_bounded_answer_is_the_one_the_same_predicate_gives_without_an_index() {
    let (store, _counting) = counted();
    let mut session = ready(&store);

    // Every bound from below the match count to above it, because a source that
    // stops early stops on a fencepost or it does not stop at all.
    for bound in [1_usize, 2, LIMIT, 399, 400, 401] {
        let served = answer(
            &mut session,
            &format!("SELECT n, rank, city FROM spans WHERE n > 0 LIMIT {bound};"),
        );
        let scanned = answer(
            &mut session,
            &format!("SELECT n, rank, city FROM mirror WHERE n > 0 LIMIT {bound};"),
        );
        assert_eq!(
            served.len(),
            scanned.len(),
            "bound {bound}: the index answered {} records and the scan {}",
            served.len(),
            scanned.len()
        );
        // Record for record, and by value rather than by identity: the two
        // tables hold different identities for the same rows, so the values are
        // what can be compared and the count above is what pins the length.
        let served: Vec<Value> = served.into_iter().map(|(_, record)| record).collect();
        let scanned: Vec<Value> = scanned.into_iter().map(|(_, record)| record).collect();
        assert_eq!(served, scanned, "bound {bound}");
    }
}

#[test]
fn a_condition_that_matches_nothing_still_costs_the_walk_it_has_to() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    // The bound cannot be filled, so there is nothing to stop for. This is the
    // control that keeps the optimisation honest: a version that "stopped" here
    // would be answering short.
    let empty = answer(
        &mut session,
        "SELECT * FROM spans WHERE n > 10000 LIMIT 10;",
    );
    assert!(empty.is_empty());

    let unfillable = examined(
        &mut session,
        &counting,
        "SELECT * FROM spans WHERE n > 0 AND rank < 0 LIMIT 10;",
    );
    let whole = examined(&mut session, &counting, "SELECT * FROM spans WHERE n > 0;");
    assert!(
        unfillable >= whole,
        "a bound that cannot be filled examined {unfillable} entries against \
         {whole} for the unbounded read — it must not stop early"
    );
}

#[test]
fn an_order_the_index_cannot_serve_takes_the_bound_away() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    // `rank` counts down while the identity counts up, and no index serves it,
    // so the first ten records the source finds are not the first ten of the
    // answer. The bound must not reach the source here, and the shape is
    // refused by the same whitelist the scan half uses rather than by a second
    // one that could drift from it.
    let ordered = ids(
        &mut session,
        "SELECT * FROM spans WHERE n > 0 ORDER BY rank LIMIT 10;",
    );
    assert_eq!(ordered.len(), LIMIT);
    // The lowest ranks are the highest identities, which the source reaches last.
    assert_eq!(ordered[0], RecordId::from(i64::try_from(RECORDS).unwrap()));

    let examined_ordered = examined(
        &mut session,
        &counting,
        "SELECT * FROM spans WHERE n > 0 ORDER BY rank LIMIT 10;",
    );
    let whole = examined(&mut session, &counting, "SELECT * FROM spans WHERE n > 0;");
    assert!(
        examined_ordered >= whole,
        "an ordered read examined {examined_ordered} entries against {whole} \
         unbounded — the order must take the bound away, not answer short"
    );
}

#[test]
fn a_bounded_index_read_answers_in_record_order_not_index_order() {
    let (store, _counting) = counted();
    let mut session = ready(&store);

    // `by_rank` is stored in the reverse of identity order, so if a bounded
    // index-served read answered in *index* order this would come back as the
    // four hundredth record and its neighbours.
    assert_eq!(
        path(&mut session, "SELECT * FROM spans WHERE rank > 0 LIMIT 5;"),
        AccessPath::Index,
    );
    let served = ids(&mut session, "SELECT * FROM spans WHERE rank > 0 LIMIT 5;");
    let scanned = ids(&mut session, "SELECT * FROM mirror WHERE rank > 0 LIMIT 5;");
    assert_eq!(served, scanned);
    assert_eq!(
        served,
        (1..=5).map(RecordId::from).collect::<Vec<_>>(),
        "a bounded index-served read must answer the records the scan answers"
    );
}
