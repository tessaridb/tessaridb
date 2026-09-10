//! The kill criterion: an expansion costs what it **matches**, not what the
//! index holds.
//!
//! This is the measurement G022 puts before any query syntax, and it is first
//! because it is the one that can stop the band. A prefix or fuzzy query that
//! walks the dictionary is not a slow feature — it is a denial of service that
//! costs the caller one keystroke, issued on exactly the query a frustrated
//! reader retries. If the cost grows with the size of the vocabulary, no amount
//! of syntax on top makes it safe, and the band is re-decided with that in
//! evidence rather than worked around.
//!
//! # What is measured, and what is the substrate's to promise
//!
//! Two halves, and conflating them would make this test agree with itself.
//!
//! **This code's half** is that the scan issued is bounded on both ends: a range
//! whose bounds are the encoded prefix, and a limit. That is asserted by the
//! entries the backend hands back staying flat as the dictionary grows a
//! hundredfold — a walk over a wider range would return more of them, and a
//! missing limit would return every matching term however many there are.
//!
//! **The substrate's half** is that a bounded range scan *seeks* rather than
//! scanning from the beginning. That is a property of an ordered store and not
//! of this crate, so asserting it by counting returned rows would be asserting
//! it about the instrument. It is measured here as elapsed time instead, against
//! the real backend: a linear substrate separates the two ends of this range by
//! about a hundredfold, and the assertion allows twenty.
//!
//! # Why the probe set is tiny and the dictionary is not
//!
//! The query matches three terms at every size. So the *answer* is identical in
//! all three runs and only the haystack changes, which is what makes a
//! difference in cost attributable to the haystack and to nothing else.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tessari_constants::SEARCH_FUZZY_EXAMINATION_CAP;
use tessari_encoding::encode_payload;
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_storage::{
    Catalog, FieldShape, IndexDefinition, IndexShape, RecordAddress, Store, TableShape,
};
use tessari_types::{Analyzer, FieldKind, Filter, Path, RecordId, Value};

/// The three vocabulary sizes, a decade apart.
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];
/// How many terms the probe matches, at every size.
const MATCHES: usize = 3;
/// The expansion cap the probe asks with — far above the matches, so a cut
/// expansion cannot be what makes the cost flat.
const CAP: usize = 64;
/// How many times the probe runs, so the elapsed time is measurable.
const REPEATS: u32 = 200;

/// A backend that answers exactly as the one beneath it and says what it handed
/// back.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    entries: AtomicUsize,
    scans: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryBackend::new()),
            entries: AtomicUsize::new(0),
            scans: AtomicUsize::new(0),
        })
    }

    fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
        self.scans.store(0, Ordering::Relaxed);
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    fn scans(&self) -> usize {
        self.scans.load(Ordering::Relaxed)
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<KvValue>> {
        self.inner.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, KvValue)>> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        let found = self.inner.scan(request)?;
        self.entries.fetch_add(found.len(), Ordering::Relaxed);
        Ok(found)
    }

    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        self.inner.count(keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.inner.apply(batch)
    }
}

struct Vocabulary {
    counting: Arc<Counting>,
    store: Store,
    index: IndexDefinition,
}

impl Vocabulary {
    /// A search index whose dictionary holds `size` distinct terms, three of
    /// which begin with `zzq`.
    ///
    /// One record per term, and one term per record: a dictionary of `size`
    /// entries is the subject, so the records are the cheapest way to produce
    /// one and their own shape is not what is being measured.
    fn of(size: usize) -> Self {
        let counting = Counting::new();
        let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "shop").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "notes", TableShape::default())
            .unwrap();
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
                    secret: false,
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

        // The probe's three terms, and then filler up to `size`. The filler is
        // spelled so that none of it can begin with `zzq` — the answer must be
        // the same three terms at every size or the runs are not comparable.
        let mut words: Vec<String> = vec![
            "zzqalpha".to_owned(),
            "zzqbeta".to_owned(),
            "zzqgamma".to_owned(),
        ];
        words.extend((0..size.saturating_sub(MATCHES)).map(|n| format!("w{n:07}")));

        // One transaction. The dictionary is the subject; how many commits built
        // it is not, and a commit per record would spend the test's whole budget
        // on the write path.
        let mut transaction = store.begin().unwrap();
        for (n, word) in words.iter().enumerate() {
            let mut fields = BTreeMap::new();
            fields.insert("body".to_owned(), Value::from(word.as_str()));
            transaction.put(
                RecordAddress::new(
                    namespace.id,
                    database.id,
                    table.id,
                    RecordId::from(format!("n{n:07}")),
                ),
                encode_payload(&Value::Object(fields)).into_bytes(),
            );
        }
        transaction.commit().unwrap();

        Self {
            counting,
            store,
            index,
        }
    }

    /// The probe, run once, with the backend counted from zero.
    fn probe(&self) -> (Vec<String>, usize, usize) {
        self.counting.reset();
        let transaction = self.store.begin().unwrap();
        let found = transaction
            .terms_with_prefix(&self.index, "zzq", CAP)
            .unwrap();
        assert!(!found.capped, "the probe was cut — the cap is too small");
        (found.terms, self.counting.entries(), self.counting.scans())
    }

    /// The **fuzzy** probe: one edit from `zzqalpha`, sharing the same three
    /// leading characters, with the backend counted from zero.
    ///
    /// Returns the terms it kept and the terms it read, which for this walk are
    /// two different numbers.
    fn fuzzy_probe(&self, word: &str) -> (Vec<String>, usize, bool, usize) {
        self.counting.reset();
        let transaction = self.store.begin().unwrap();
        let found = transaction
            .terms_within_distance(&self.index, word, 2, 3, CAP)
            .unwrap();
        (
            found.terms,
            found.examined,
            found.capped,
            self.counting.entries(),
        )
    }

    /// The probe repeated, timed.
    fn timed(&self) -> Duration {
        let transaction = self.store.begin().unwrap();
        let start = Instant::now();
        for _ in 0..REPEATS {
            let found = transaction
                .terms_with_prefix(&self.index, "zzq", CAP)
                .unwrap();
            assert_eq!(found.terms.len(), MATCHES);
        }
        start.elapsed()
    }
}

#[test]
fn an_expansion_costs_what_it_matches_and_not_what_the_index_holds() {
    let mut readings = Vec::new();
    for size in SIZES {
        let vocabulary = Vocabulary::of(size);
        let (terms, entries, scans) = vocabulary.probe();
        assert_eq!(
            terms,
            vec![
                "zzqalpha".to_owned(),
                "zzqbeta".to_owned(),
                "zzqgamma".to_owned()
            ],
            "the answer changed with the vocabulary size — the runs are not comparable"
        );
        let elapsed = vocabulary.timed();
        readings.push((size, entries, scans, elapsed));
    }

    for (size, entries, scans, elapsed) in &readings {
        println!("terms={size} entries={entries} scans={scans} elapsed={elapsed:?}");
    }

    // This code's half: the walk hands back the matches and nothing else, at
    // every size. A range wider than the prefix would grow with the vocabulary;
    // a missing limit would return every match however many there were.
    let (_, first_entries, first_scans, first_elapsed) = readings[0];
    for (size, entries, scans, _) in &readings {
        assert_eq!(
            *entries, first_entries,
            "the walk read {entries} entries at {size} terms and {first_entries} at {}",
            readings[0].0
        );
        assert_eq!(*scans, first_scans, "the walk issued more scans at {size}");
    }
    assert!(
        first_entries <= MATCHES.saturating_add(1),
        "the walk read {first_entries} entries to find {MATCHES} terms"
    );

    // The substrate's half: a bounded range scan seeks. A linear one separates
    // the ends of this range by about a hundredfold; twenty is the allowance,
    // wide enough that only a genuinely linear walk fails it.
    let (largest, _, _, last_elapsed) = readings[readings.len().saturating_sub(1)];
    let floor = Duration::from_micros(50).max(first_elapsed);
    assert!(
        last_elapsed < floor.saturating_mul(20),
        "the walk took {last_elapsed:?} at {largest} terms against {first_elapsed:?} at {} — \
         the cost is growing with the vocabulary",
        readings[0].0
    );
}

#[test]
fn an_expansion_that_runs_out_of_room_says_so_rather_than_reporting_a_subset() {
    // The flag is the whole difference between "the words beginning with this"
    // and "the first n words beginning with this". A caller that got exactly the
    // cap and no signal would report the second as the first.
    let vocabulary = Vocabulary::of(1_000);
    let transaction = vocabulary.store.begin().unwrap();

    let room = transaction
        .terms_with_prefix(&vocabulary.index, "zzq", 5)
        .unwrap();
    assert_eq!(room.terms.len(), MATCHES);
    assert!(!room.capped);

    // Exactly the cap: the boundary a naive implementation reports as complete.
    let exact = transaction
        .terms_with_prefix(&vocabulary.index, "zzq", MATCHES)
        .unwrap();
    assert_eq!(exact.terms.len(), MATCHES);
    assert!(!exact.capped, "a complete expansion was reported as cut");

    let cut = transaction
        .terms_with_prefix(&vocabulary.index, "zzq", 2)
        .unwrap();
    assert_eq!(cut.terms.len(), 2);
    assert!(cut.capped, "a cut expansion was reported as complete");

    // And a prefix nothing begins with is an empty answer rather than a cut one.
    let none = transaction
        .terms_with_prefix(&vocabulary.index, "qqq", CAP)
        .unwrap();
    assert!(none.terms.is_empty());
    assert!(!none.capped);
}

/// **G022 S3, for the fuzzy walk rather than the prefix one.**
///
/// The prefix walk's PASS is not this criterion's evidence and the goal's ledger
/// now says so. For a prefix, the range *is* the answer: everything read is
/// returned, so "terms examined" and "terms returned" are one number and a test
/// that measured either would measure both. A fuzzy walk reads every term
/// sharing the mandatory prefix and keeps only those inside the edit budget, so
/// the two numbers come apart — and S3 is a claim about the one that could grow.
///
/// So both are measured, separately, at each size.
#[test]
fn a_fuzzy_expansion_reads_its_prefix_range_and_keeps_what_the_budget_allows() {
    let mut readings = Vec::new();
    for size in SIZES {
        let vocabulary = Vocabulary::of(size);
        let (terms, examined, capped, entries) = vocabulary.fuzzy_probe("zzqalph");
        assert!(!capped, "the probe was cut at {size} terms");
        assert_eq!(
            terms,
            vec!["zzqalpha".to_owned()],
            "the answer changed with the vocabulary size — the runs are not comparable"
        );
        readings.push((size, terms.len(), examined, entries));
    }

    for (size, returned, examined, entries) in &readings {
        println!("terms={size} returned={returned} examined={examined} entries={entries}");
    }

    // Neither number moves with the dictionary. The kill criterion does not fire.
    let (_, first_returned, first_examined, first_entries) = readings[0];
    for (size, returned, examined, entries) in &readings {
        assert_eq!(*returned, first_returned, "the answer changed at {size}");
        assert_eq!(
            *examined, first_examined,
            "the walk read {examined} terms at {size} and {first_examined} at {}",
            readings[0].0
        );
        assert_eq!(*entries, first_entries, "the backend read more at {size}");
    }

    // And the honest half: examined exceeds returned. Asserting this rather than
    // letting them be coincidentally equal is what stops a later change from
    // making `examined` a copy of `terms.len()` and the measurement vacuous.
    assert!(
        first_examined > first_returned,
        "examined ({first_examined}) did not exceed returned ({first_returned}), \
         so the two numbers are not being counted separately"
    );
    assert_eq!(
        first_examined, MATCHES,
        "the walk should read exactly the terms sharing the prefix"
    );
}

/// **What the bound actually is**, stated by measurement rather than by hope.
///
/// The test above holds the prefix range at three terms, so it shows the walk
/// does not read the dictionary. It does not show what the walk *does* read
/// with, and the honest answer is: the number of terms sharing the mandatory
/// prefix — a property of the corpus, not of the query's match count. That is
/// the price of intersecting an edit-distance automaton with a dictionary by
/// walking a range instead of by seeking.
///
/// Two ceilings stop that, and they stop **different shapes**, which is why both
/// exist and why both are exercised here:
///
/// - many terms *match* — the expansion cap ends the walk as soon as the union
///   would be wider than the index is willing to read;
/// - many terms are *read* and few match — nothing above would ever fire, and
///   `SEARCH_FUZZY_EXAMINATION_CAP` is the only thing between a popular
///   three-letter beginning and a walk of most of the vocabulary.
///
/// Neither is a refusal. A capped expansion is simply not offered as a
/// candidate, and the scan answers with the identical result.
#[test]
fn a_popular_prefix_is_declined_rather_than_walked() {
    let vocabulary = Vocabulary::of(100_000);

    // Shape one: the neighbours of a real term are themselves terms, so matches
    // pile up and the expansion cap ends it. 99_997 terms share this prefix.
    let (terms, examined, capped, _) = vocabulary.fuzzy_probe("w0000000");
    println!(
        "matching: returned={} examined={examined} capped={capped}",
        terms.len()
    );
    assert!(
        capped,
        "a prefix reaching most of the vocabulary was walked to the end"
    );
    assert_eq!(
        terms.len(),
        CAP,
        "the walk should stop at the expansion cap"
    );
    assert!(
        examined <= CAP.saturating_add(1),
        "the walk read {examined} terms before its {CAP} matches"
    );

    // Shape two: the same popular prefix, and a word nothing is within two edits
    // of. No match ever accumulates, so only the examination ceiling can end it.
    let (terms, examined, capped, _) = vocabulary.fuzzy_probe("w00zzzzz");
    println!(
        "non-matching: returned={} examined={examined} capped={capped}",
        terms.len()
    );
    assert!(terms.is_empty(), "nothing should be within two edits");
    assert!(
        capped,
        "a walk that matched nothing read the range to the end instead of declining"
    );
    assert_eq!(
        examined, SEARCH_FUZZY_EXAMINATION_CAP,
        "the walk did not stop at its examination ceiling"
    );
}
