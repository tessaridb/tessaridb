//! A `LIMIT` that reaches the source, and every shape where it must not.
//!
//! `SELECT * FROM spans LIMIT 10` over fifty thousand records used to read and
//! hold fifty thousand. `START` and `LIMIT` are applied last — after the source
//! is materialised, after `FETCH`, after the projection and after the sort — so
//! the bound was a truncation of the answer rather than an instruction to the
//! source. At the measured 887 bytes an answered record that is forty-three
//! megabytes to answer with ten (ADR-0013).
//!
//! # What is asserted here, and why it is entries and not milliseconds
//!
//! The counting backend reports the rows the scans handed back. A read that
//! answers the right ten records having scanned all four hundred passes every
//! other test in this crate — the cost is invisible to all of them, and a
//! timing would report it as noise on a table this size. Entries returned is
//! the quantity the promise is actually about.
//!
//! # The half that matters more than the optimisation
//!
//! The bound is pushed only for statement shapes known to preserve the record
//! count, and this file spends most of its length on the shapes where it must
//! **not** be. A grouping folds many records into one, so its limit counts
//! groups; an ordering the source cannot serve decides which records survive, so
//! taking the source's first ten and sorting those is a different answer.
//!
//! Both failures are **quietly short answers**: not an error, not a crash, just
//! fewer records than the caller asked about, returned confidently. So the
//! refusals are exercised rather than assumed — a test that believed it took the
//! bounded path when it did not would pass for the wrong reason, which is the
//! one way this file could be worse than useless.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bgv_db_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use bgv_db_session::{Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

/// How many records the table holds.
///
/// Large enough that a bounded read and an unbounded one cannot be confused by
/// a fencepost, small enough that every test here is instant.
const RECORDS: u64 = 400;

/// The bound every limited read in this file asks for.
const LIMIT: usize = 10;

/// A backend that answers exactly as the one beneath it and counts the rows it
/// handed back.
///
/// Rows rather than calls: one scan that returned the whole table is a single
/// call and a linear cost, so a call count would report the change as having
/// achieved nothing and a row count reports what it did.
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

/// A table of `RECORDS` records, each with a value that is not its identity.
///
/// `rank` counts down while the identity counts up, so identity order and value
/// order are different orders — otherwise an ordering served by taking the
/// source's first *n* would accidentally be right, and the test that exists to
/// catch exactly that would not.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE spans;",
        )
        .unwrap();
    for n in 1..=RECORDS {
        session
            .run(&format!(
                "CREATE spans:{n} = {{ n: {n}, rank: {}, city: 'city {}' }};",
                RECORDS.saturating_sub(n),
                n % 4
            ))
            .unwrap();
    }
    session
}

/// The identities a read answers with, in the order it answered.
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

/// How many entries one read costs, with the counter zeroed first.
fn cost(session: &mut Session<'_>, counting: &Counting, read: &str) -> usize {
    counting.reset();
    session.run(read).unwrap();
    counting.entries()
}

#[test]
fn a_limited_read_costs_entries_proportional_to_the_limit_and_not_to_the_table() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    let bounded = cost(
        &mut session,
        &counting,
        &format!("SELECT * FROM spans LIMIT {LIMIT};"),
    );
    let whole = cost(&mut session, &counting, "SELECT * FROM spans;");
    let records = usize::try_from(RECORDS).unwrap();

    // The difference rather than the level. Both statements resolve the same
    // namespace, database and table before they read anything, and that catalog
    // cost is identical in each — so subtracting one from the other cancels it
    // and leaves an exact equality about the records, where an assertion on the
    // level would have to carry a fudge factor and would stop meaning anything
    // the day the catalog changed shape.
    assert_eq!(
        whole - bounded,
        records - LIMIT,
        "the bounded read cost {bounded} entries and the whole table {whole}; \
         the difference should be the {} records not read",
        records - LIMIT
    );
    assert!(
        bounded < whole,
        "the bounded read cost {bounded} and the unbounded one {whole}"
    );
}

#[test]
fn the_bounded_read_answers_exactly_the_prefix_the_unbounded_one_answers() {
    let (store, _) = counted();
    let mut session = ready(&store);

    let bounded = answer(&mut session, &format!("SELECT * FROM spans LIMIT {LIMIT};"));
    let whole = answer(&mut session, "SELECT * FROM spans;");

    assert_eq!(bounded.len(), LIMIT);
    assert_eq!(
        bounded,
        whole[..LIMIT].to_vec(),
        "the bounded read answered different records, not fewer"
    );
}

#[test]
fn a_start_is_part_of_the_bound_rather_than_applied_after_it() {
    let (store, _) = counted();
    let mut session = ready(&store);

    let paged = answer(
        &mut session,
        &format!("SELECT * FROM spans START 5 LIMIT {LIMIT};"),
    );
    let whole = answer(&mut session, "SELECT * FROM spans;");

    assert_eq!(
        paged,
        whole[5..5 + LIMIT].to_vec(),
        "a bound that forgot its start answers the wrong page"
    );
}

/// The refusals. Each of these limits something other than the records the
/// source produces, so a bound pushed into the source would answer short.
#[test]
fn a_grouping_reads_everything_because_its_limit_counts_groups() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    let cost = cost(
        &mut session,
        &counting,
        &format!("SELECT city FROM spans GROUP BY city LIMIT {LIMIT};"),
    );
    assert!(
        cost >= usize::try_from(RECORDS).unwrap(),
        "a grouped read cost {cost} entries and the table holds {RECORDS} — \
         the bound reached a source whose records are not the answer's records"
    );
}

#[test]
fn a_fold_reads_everything_because_its_answer_is_one_record() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    let cost = cost(
        &mut session,
        &counting,
        &format!("SELECT count(*) AS total FROM spans LIMIT {LIMIT};"),
    );
    assert!(
        cost >= usize::try_from(RECORDS).unwrap(),
        "a folded read cost {cost} entries and the table holds {RECORDS}"
    );
}

#[test]
fn an_ordering_the_source_does_not_serve_reads_everything() {
    let (store, counting) = counted();
    let mut session = ready(&store);

    let cost = cost(
        &mut session,
        &counting,
        &format!("SELECT * FROM spans ORDER BY rank LIMIT {LIMIT};"),
    );
    assert!(
        cost >= usize::try_from(RECORDS).unwrap(),
        "an ordered read cost {cost} entries and the table holds {RECORDS}"
    );
}

/// The one the refusal exists for: `rank` counts down while identity counts up,
/// so the first ten by identity and the first ten by `rank` share no record.
/// A bound pushed past this refusal answers ten real records that are the wrong
/// ten — which is the failure this whole file is shaped around, because it looks
/// exactly like a correct answer.
#[test]
fn an_ordered_read_answers_the_records_the_order_chose_and_not_the_first_ten_found() {
    let (store, _) = counted();
    let mut session = ready(&store);

    let ordered = ids(
        &mut session,
        &format!("SELECT * FROM spans ORDER BY rank LIMIT {LIMIT};"),
    );
    let unordered = ids(&mut session, &format!("SELECT * FROM spans LIMIT {LIMIT};"));

    assert_eq!(ordered.len(), LIMIT);
    assert_eq!(
        ordered[0],
        RecordId::Int(i64::try_from(RECORDS).unwrap()),
        "the lowest rank belongs to the last record written"
    );
    assert!(
        ordered.iter().all(|id| !unordered.contains(id)),
        "the ordered answer and the unordered one share a record, so this test \
         could pass with the ordering ignored"
    );
}

/// An uncommitted **tombstone** removes a record the walk already counted, so a
/// bound that stopped at exactly *n* answers *n - 1*: the right records, one
/// fewer of them, with nothing raised. That is what the contract's over-fetch
/// exists to prevent.
///
/// **Deletes only, and that is the point.** The first version of this test used
/// one insert and one delete, and it passed with the over-fetch removed —
/// because an insert adds a record at the front while a delete takes one from
/// the middle, and the two cancelled. It was a test of nothing, and only a
/// falsification run found that out. Inserts genuinely cannot displace: they can
/// only make the set larger, and the caller's own bound truncates it. Three
/// deletes rather than one, so the assertion is about a quantity and not about
/// a single fencepost.
#[test]
fn a_bound_survives_uncommitted_deletes_that_remove_what_it_stopped_at() {
    let (store, _) = counted();
    let mut session = ready(&store);

    // One script, because a transaction must open and close inside a single
    // call. Both reads therefore run at the same uncommitted state, which is
    // what the comparison needs anyway.
    let outcomes = session
        .run(&format!(
            "BEGIN;\n\
             DELETE spans:3;\n\
             DELETE spans:5;\n\
             DELETE spans:8;\n\
             SELECT * FROM spans LIMIT {LIMIT};\n\
             SELECT * FROM spans;\n\
             CANCEL;"
        ))
        .unwrap();
    let records = |outcome: &Outcome| outcome.records().unwrap().to_vec();
    let bounded = records(&outcomes[4]);
    let whole = records(&outcomes[5]);

    assert_eq!(
        bounded.len(),
        LIMIT,
        "the bound answered {} records where {LIMIT} were available",
        bounded.len()
    );
    assert_eq!(
        bounded,
        whole[..LIMIT].to_vec(),
        "pending deletes displaced the records the bound stopped at"
    );
}

/// A limit larger than the table answers the table, rather than waiting for
/// records that do not exist.
#[test]
fn a_limit_past_the_end_answers_everything_there_is() {
    let (store, _) = counted();
    let mut session = ready(&store);

    let asked = answer(
        &mut session,
        &format!("SELECT * FROM spans LIMIT {};", RECORDS * 2),
    );
    assert_eq!(asked.len(), usize::try_from(RECORDS).unwrap());
}

/// `FETCH` maps one record to one record, so it preserves the count and the
/// bound is allowed through it.
#[test]
fn a_fetch_preserves_the_count_and_the_bound_still_applies() {
    let (store, _) = counted();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes;\n\
             CREATE notes:1 = { about: spans:1, body: 'a note' };\n\
             CREATE notes:2 = { about: spans:2, body: 'another' };\n\
             CREATE notes:3 = { about: spans:3, body: 'a third' };",
        )
        .unwrap();

    let bounded = answer(&mut session, "SELECT * FROM notes FETCH about LIMIT 2;");
    let whole = answer(&mut session, "SELECT * FROM notes FETCH about;");

    assert_eq!(bounded.len(), 2);
    assert_eq!(bounded, whole[..2].to_vec());
}

/// The outcome shape is unchanged by any of this: a read still answers records
/// and a path, not a new kind of answer (ADR-0013 §5).
#[test]
fn a_bounded_read_still_answers_a_materialised_value() {
    let (store, _) = counted();
    let mut session = ready(&store);
    let outcome = session
        .run(&format!("SELECT * FROM spans LIMIT {LIMIT};"))
        .unwrap();
    assert!(matches!(
        outcome.last(),
        Some(Outcome::Records { records, .. }) if records.len() == LIMIT
    ));
}
