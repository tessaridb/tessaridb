//! A bounded ranked read, and the two things it has to be true about.
//!
//! A `SELECT … ORDER BY search::score(f, q) DESC LIMIT k` no longer scores every
//! record in the table. It enumerates the postings of the query's own terms, and
//! it stops enumerating a term once the most that term and everything after it
//! could contribute falls below the score already sitting in `k`th place.
//!
//! Both halves of that need asserting, and they fail in opposite directions:
//!
//! - **Correctness.** The pruned read must answer exactly what scoring the whole
//!   table answers. Nothing about a wrong answer here looks wrong — the rows come
//!   back, they are plausible, they are in a sensible order, and the ones that
//!   are missing were never mentioned. So the reference is the unbounded read,
//!   which takes the scan, and the assertion is record-for-record equality.
//! - **Cost.** A walk that quietly stopped pruning would pass every equality
//!   above and cost what it always did. So one test compares two reads over the
//!   same data whose costs invert if the term suffix is never abandoned.
//!
//! This is G014 F5's pruning half: the safety of any pruning is established by
//! equivalence against brute-force scoring on the same data, and never by a
//! timing comparison — a faster wrong answer is the failure mode, and a benchmark
//! cannot see it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

/// How many records the table holds.
const RECORDS: u64 = 60;

/// How many of them hold the rare word.
const RARE: u64 = 4;

/// A backend that answers exactly as the one beneath it and counts what it
/// handed back.
///
/// Rows rather than calls, for the reason `bounded_reads` gives: one scan that
/// returned the whole table is a single call and a linear cost.
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

fn counted() -> (Store, Arc<Counting>) {
    let inner = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let counting = Arc::new(Counting::new(inner));
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    (store, counting)
}

/// `notes`, with a search index over an analysed `body`.
///
/// **Every** record holds `note`, and four of them hold `quorum` a differing
/// number of times. That shape is the point: a word in every document carries
/// almost no weight and has a posting list as long as the table, which is
/// exactly the term a bounded read should decide not to read.
fn searchable(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
        )
        .unwrap();
    for n in 1..=RECORDS {
        let body = if n <= RARE {
            // Different counts, so the four are ordered among themselves by
            // something the walk has to compute rather than by identity.
            let quorums = vec!["quorum"; usize::try_from(n).unwrap()].join(" ");
            format!("a note about {quorums}")
        } else {
            format!("a note number {n} about nothing in particular")
        };
        session
            .run(&format!("CREATE notes:{n} = {{ body: '{body}' }};"))
            .unwrap();
    }
    session
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

/// What the plan says a read takes.
fn plan(session: &mut Session<'_>, read: &str) -> Value {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    match outcomes.last().unwrap() {
        tessari_session::Outcome::Value(held) => held.clone(),
        other => panic!("not a plan: {other:?}"),
    }
}

fn shape(plan: &Value) -> Option<String> {
    let Value::Object(fields) = plan else {
        panic!("not a plan: {plan:?}");
    };
    match fields.get("shape") {
        Some(Value::String(held)) => Some(held.clone()),
        _ => None,
    }
}

fn access(plan: &Value) -> String {
    let Value::Object(fields) = plan else {
        panic!("not a plan: {plan:?}");
    };
    match fields.get("access") {
        Some(Value::String(held)) => held.clone(),
        other => panic!("no access path: {other:?}"),
    }
}

/// The reference: no bound, so no pruning, so the scan scores every record.
const WHOLE: &str = "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC;";

#[test]
fn a_bounded_scored_read_answers_what_scoring_the_whole_table_answers() {
    let (store, _counting) = counted();
    let mut session = searchable(&store);

    let brute = answer(&mut session, WHOLE);
    assert_eq!(brute.len(), usize::try_from(RECORDS).unwrap());

    // Every bound the pruning path serves. `RARE` is the interesting boundary:
    // below it the walk stops inside the rare term, at it the bound is filled
    // exactly, and above it there are not enough postings to fill the answer.
    for wanted in 1..=usize::try_from(RARE).unwrap() {
        let bounded = answer(
            &mut session,
            &format!(
                "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC LIMIT {wanted};"
            ),
        );
        assert_eq!(bounded, brute[..wanted], "at LIMIT {wanted}");
    }
}

#[test]
fn a_start_is_part_of_the_bound_and_not_a_reason_to_refuse_one() {
    let (store, _counting) = counted();
    let mut session = searchable(&store);

    let brute = answer(&mut session, WHOLE);
    let bounded = answer(
        &mut session,
        "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC \
         START 2 LIMIT 2;",
    );
    assert_eq!(bounded, brute[2..4]);
}

#[test]
fn a_bound_the_postings_cannot_fill_falls_back_to_the_scan() {
    // Past `RARE` the answer needs records holding none of the query's words.
    // Their order among themselves is the scan's, so the walk declines and the
    // scan produces it — and the answer is still the brute-force one.
    let (store, _counting) = counted();
    let mut session = searchable(&store);

    let brute = answer(&mut session, WHOLE);
    let wanted = usize::try_from(RARE).unwrap() + 6;
    let bounded = answer(
        &mut session,
        &format!(
            "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC LIMIT {wanted};"
        ),
    );
    assert_eq!(bounded, brute[..wanted]);
}

#[test]
fn a_bounded_scored_read_costs_far_less_than_scoring_the_table() {
    let (store, counting) = counted();
    let mut session = searchable(&store);

    let bounded = cost(
        &mut session,
        &counting,
        "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC LIMIT 2;",
    );
    let whole = cost(&mut session, &counting, WHOLE);

    // Fewer entries than the table has records, which is the whole claim: the
    // read stopped being proportional to the table. The level rather than the
    // ratio, because both reads also resolve the same catalog and the same
    // collection statistics, and that fixed part is not what this asserts.
    let records = usize::try_from(RECORDS).unwrap();
    assert!(
        bounded < records,
        "bounded read cost {bounded} entries over {records} records (the scan cost {whole})"
    );
}

#[test]
fn a_term_that_cannot_reach_the_answer_has_its_postings_left_unread() {
    // The cost assertion that inverts if the walk stops abandoning term
    // suffixes. `note` is in every record, so its bound is nearly nothing and
    // its posting list is the whole table.
    //
    // Adding it to a query for `quorum` must cost **less** than asking for it
    // alone, because the four `quorum` postings fill the bound first and the
    // suffix holding only `note` then cannot reach the score in last place. A
    // walk that read every term would cost the sum of the two and could not come
    // in under either.
    let (store, counting) = counted();
    let mut session = searchable(&store);

    let both = cost(
        &mut session,
        &counting,
        "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC LIMIT 2;",
    );
    let common = cost(
        &mut session,
        &counting,
        "SELECT * FROM notes ORDER BY search::score(body, 'note') DESC LIMIT 2;",
    );

    assert!(
        both < common,
        "reading both terms cost {both} entries and the common one alone cost {common} — \
         the suffix was not abandoned"
    );
}

/// Two words of equal weight, held by different records, both belonging in the
/// answer.
///
/// The fixture above cannot catch an over-aggressive bound: the four `quorum`
/// records out-score the other fifty-six by so much that pruning ten times too
/// hard still only discards records that could never have won. A bound that is
/// wrong in the unsafe direction has to be caught where the decision is close,
/// so here the two terms are in ten records each and neither group can be
/// dropped without losing rows the answer wants.
fn contested(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE pairs SCHEMALESS;\n\
             DEFINE FIELD body ON pairs TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_body ON pairs FIELDS body SEARCH;",
        )
        .unwrap();
    for n in 1..=20_u64 {
        let word = if n <= 10 { "alpha" } else { "beta" };
        let repeats = usize::try_from(n % 4).unwrap().saturating_add(1);
        let body = vec![word; repeats].join(" ");
        session
            .run(&format!("CREATE pairs:{n} = {{ body: '{body}' }};"))
            .unwrap();
    }
    session
}

#[test]
fn a_bound_that_prunes_a_term_holding_answers_is_caught() {
    let (store, _counting) = counted();
    let mut session = contested(&store);

    let brute = answer(
        &mut session,
        "SELECT * FROM pairs ORDER BY search::score(body, 'alpha beta') DESC;",
    );
    // The premise: the top of the answer holds both words' records. Asserted
    // rather than assumed, because a fixture that quietly put one group on top
    // would make the equality below pass for the wrong reason.
    let leading: Vec<_> = brute[..6].iter().map(|(id, _)| id.clone()).collect();
    let alphas = leading
        .iter()
        .filter(|id| matches!(id, RecordId::Int(n) if *n <= 10))
        .count();
    assert!(
        alphas > 0 && alphas < leading.len(),
        "the fixture does not contest the bound: {leading:?}"
    );

    for wanted in 1..=6 {
        let bounded = answer(
            &mut session,
            &format!(
                "SELECT * FROM pairs ORDER BY search::score(body, 'alpha beta') DESC LIMIT {wanted};"
            ),
        );
        assert_eq!(bounded, brute[..wanted], "at LIMIT {wanted}");
    }
}

#[test]
fn records_that_score_the_same_are_ordered_by_the_answer_and_not_by_the_walk() {
    // Every equivalence above compares reads whose scores are all different, so
    // a tie broken the wrong way would pass all of them. Twelve records with
    // identical text score identically, and the ordering stage breaks the tie by
    // record id — a **total** order, which is what makes the answer the same
    // whatever access path produced the candidates.
    //
    // Asserted here rather than taken from `shape.rs`'s own tests because this
    // is the property the new walk depends on: it hands back its candidates in
    // its own order and never sorts them, so if ties were settled by production
    // order instead, adding this path would silently reorder equal rows.
    let (store, _counting) = counted();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
        )
        .unwrap();
    // Written out of identity order, so a walk that happened to produce records
    // in the order they were created would not accidentally agree with the scan.
    for n in [7_u64, 3, 11, 1, 9, 5, 12, 2, 8, 4, 10, 6] {
        session
            .run(&format!("CREATE notes:{n} = {{ body: 'quorum quorum' }};"))
            .unwrap();
    }

    let brute = answer(
        &mut session,
        "SELECT * FROM notes ORDER BY search::score(body, 'quorum') DESC;",
    );
    for wanted in [1_usize, 3, 6] {
        let bounded = answer(
            &mut session,
            &format!(
                "SELECT * FROM notes ORDER BY search::score(body, 'quorum') DESC LIMIT {wanted};"
            ),
        );
        assert_eq!(bounded, brute[..wanted], "at LIMIT {wanted}");
    }
}

#[test]
fn a_pruned_read_calls_itself_exact_and_is_entitled_to() {
    // ADR-0049: a path that cannot prove itself has to say so. This one can, and
    // the entitlement is the equivalence asserted above rather than the match arm
    // that happens to map `ordered` to exact — so the claim is pinned here, where
    // the proof is, and a walk that stopped being equivalent would leave a test
    // asserting `exact: true` beside tests showing it is not.
    let (store, _counting) = counted();
    let mut session = searchable(&store);

    let outcomes = session
        .run("SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC LIMIT 2;")
        .unwrap();
    let Some(tessari_session::Outcome::Records { plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    assert_eq!(plan.exact, tessari_session::Exactness::Exact);
    assert_eq!(plan.shape, Some("scored"));
}

#[test]
fn the_plan_names_the_walk_rather_than_hiding_it_inside_ordered() {
    let (store, _counting) = counted();
    let mut session = searchable(&store);

    let bounded = plan(
        &mut session,
        "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC LIMIT 2",
    );
    assert_eq!(access(&bounded), "ordered");
    assert_eq!(shape(&bounded).as_deref(), Some("scored"));

    // Unbounded, so there is nothing to prune against and the read is the scan.
    // The plan has to say so: a plan that reported the walk for a read that
    // scans would disarm the one instrument that can see a candidate set widen.
    let whole = plan(
        &mut session,
        "SELECT * FROM notes ORDER BY search::score(body, 'quorum note') DESC",
    );
    assert_eq!(access(&whole), "scan");
    assert_eq!(shape(&whole), None);
}

/// The projected spelling of the same read: a title, a score under a name, and
/// the bound.
///
/// Q-384. The recognizer used to refuse every projection, inheriting the reason
/// `plan::statement::ordered` gives — that a sort key may name what the
/// projection produced. Measured (`tests/ranking.rs`), that reason does not
/// reach a score: the number comes from the postings and the record's identity,
/// and where a key does read the record the ordering stage lays the source
/// beneath the projection. So the shape below is the one a caller actually
/// writes, and it now takes the bound.
const PROJECTED: &str = "SELECT title, search::score(body, 'alpha beta') AS score FROM pairs \
                         ORDER BY search::score(body, 'alpha beta') DESC LIMIT 4";

#[test]
fn a_projected_ranked_read_takes_the_bound() {
    let (store, _counting) = counted();
    let mut session = contested(&store);

    let projected = plan(&mut session, PROJECTED);
    assert_eq!(access(&projected), "ordered");
    assert_eq!(shape(&projected).as_deref(), Some("scored"));

    // The half that catches a widening rather than a narrowing. An equality
    // against the scan can only see the bound reading too few records; it passes
    // unchanged if the walk quietly stops taking the bound at all (KB 294), and
    // this is what would fail then.
    let starred = plan(
        &mut session,
        "SELECT * FROM pairs ORDER BY search::score(body, 'alpha beta') DESC LIMIT 4",
    );
    assert_eq!(shape(&starred), shape(&projected));
}

#[test]
fn a_projected_ranked_read_answers_what_the_scan_answers() {
    let (store, _counting) = counted();
    let mut session = contested(&store);

    let brute = answer(
        &mut session,
        "SELECT * FROM pairs ORDER BY search::score(body, 'alpha beta') DESC;",
    );
    // The fixture has to contest the bound or the equality below passes on data
    // where nothing could have gone wrong — the lesson this band has paid for
    // twice. Both words' records belong in the top of the answer, so neither
    // group can be pruned away without losing rows.
    let leading: Vec<_> = brute[..4].iter().map(|(id, _)| id.clone()).collect();
    let alphas = leading
        .iter()
        .filter(|id| matches!(id, RecordId::Int(n) if *n <= 10))
        .count();
    assert!(
        alphas > 0 && alphas < leading.len(),
        "the fixture does not contest the bound: {leading:?}"
    );

    // Ids rather than records: the two reads answer in different shapes on
    // purpose, and which records the bound reached is the question.
    let bounded: Vec<_> = answer(&mut session, &format!("{PROJECTED};"))
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(bounded, leading);
}

#[test]
fn a_projection_shadowing_the_searched_field_is_refused_the_bound() {
    // The narrowing that makes the relaxation safe, pinned as its own test. A
    // projection answering under the searched field's name with something else
    // replaces what the ordering stage reads, and the overlay does not undo it —
    // the projection is meant to win on a name it offers. So this shape declines
    // to the scan, and a later widening that admitted it would fail here rather
    // than in a wrong answer nobody looks at.
    let (store, _counting) = counted();
    let mut session = contested(&store);

    let shadowing = plan(
        &mut session,
        "SELECT 'x' AS body FROM pairs \
         ORDER BY search::score(body, 'alpha beta') DESC LIMIT 4",
    );
    assert_eq!(access(&shadowing), "scan");
    assert_eq!(shape(&shadowing), None);

    // And the field written out under its own name is not a shadow: it is the
    // same value the key would have read, so the read keeps its bound.
    let keeping = plan(
        &mut session,
        "SELECT body, search::score(body, 'alpha beta') AS score FROM pairs \
         ORDER BY search::score(body, 'alpha beta') DESC LIMIT 4",
    );
    assert_eq!(access(&keeping), "ordered");
    assert_eq!(shape(&keeping).as_deref(), Some("scored"));
}
