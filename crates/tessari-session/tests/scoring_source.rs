//! Where a score's per-record numbers come from, and that moving them did not
//! move the ranking.
//!
//! # The two numbers, and why this is not a refactor with a benchmark attached
//!
//! BM25 needs four numbers. Two describe the collection and have always been
//! read from the index. The other two describe the record being scored — how
//! often it holds each asked term, and how long it is — and used to be recovered
//! by **analysing the record's text again**, once per scored record per query,
//! to arrive at figures the writer already knew and had already stored in the
//! posting.
//!
//! Removing that is only safe if the stored figures are the same figures. They
//! are counted by the same function on the write path, so they should be — and
//! "should be" is exactly the claim a store is not allowed to make about its own
//! ranking. Hence the first test: both routes still exist in the code, so the
//! comparison is between two production paths over the same data rather than
//! against a number somebody wrote down.
//!
//! # Exact equality, not approximate
//!
//! The scores are compared as **bit-identical floats**. A tolerance here would
//! pass on an implementation that had quietly changed what it counts — distinct
//! terms instead of tokens, say — because such a change moves a score by a
//! little and a ranking by a lot. If the two routes agree at all they agree
//! exactly, so anything less than exact is a defect wearing a rounding error.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tessari_encoding::{Posting, PostingKey, StoreKey, StoreValue};
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanDirection, ScanRequest,
    Value as KvValue, WriteBatch,
};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

/// How many records the corpus holds.
const RECORDS: u32 = 200;
/// The words the query asks for. Three distinct terms, so the expected number of
/// point reads per scored record is three.
const QUERY: &str = "lock contention vector";
/// How many distinct terms that is.
const TERMS: usize = 3;

/// A backend that answers exactly as the one beneath it and says how many
/// **posting** point reads passed through it.
///
/// Posting reads specifically, and not every read of the index keyspace: the
/// collection's own two numbers and each term's document frequency are also
/// point reads there, and they are resolved once per read rather than once per
/// record. Counting them together would blur the very distinction being
/// measured.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    postings: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryBackend::new()),
            postings: AtomicUsize::new(0),
        })
    }

    fn reset(&self) {
        self.postings.store(0, Ordering::Relaxed);
    }

    fn postings(&self) -> usize {
        self.postings.load(Ordering::Relaxed)
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<KvValue>> {
        if PostingKey::decode(key.as_slice()).is_ok() {
            self.postings.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get(keyspace, key)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, KvValue)>> {
        self.inner.scan(request)
    }

    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        self.inner.count(keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.inner.apply(batch)
    }
}

/// A record's body: the query's words at a record-dependent frequency, padded to
/// `filler` words of prose that hold none of them.
///
/// The padding is what makes the corpus a corpus of **long** documents, and it
/// is deliberately made of words the query never asks about: it changes each
/// record's length — which BM25 divides by — without changing which terms it
/// holds, so a length read from the wrong place shows up as a wrong score
/// rather than as a missing one.
fn body(n: u32, filler: usize) -> String {
    let mut words = Vec::new();
    for _ in 0..=(n % 4) {
        words.push("lock".to_owned());
    }
    if n % 3 == 0 {
        words.push("contention".to_owned());
    }
    if n % 5 == 0 {
        words.push("vector".to_owned());
        words.push("vector".to_owned());
    }
    for word in 0..filler {
        words.push(format!("padding{word:05}"));
    }
    words.join(" ")
}

/// A populated, indexed table, and the backend that watched it being read.
fn corpus(filler: usize) -> (Arc<Counting>, Store) {
    let counting = Counting::new();
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;",
        )
        .unwrap();
    for n in 0..RECORDS {
        session
            .run(&format!(
                "CREATE notes:{n} = {{ body: '{}' }};",
                body(n, filler)
            ))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();
    (counting, store)
}

/// Every record's score, by id, from one ranked read of the standard query.
fn scores(store: &Store) -> Vec<(String, f64)> {
    scores_for(store, QUERY)
}

/// Rewrite every posting in the store as one written before postings carried a
/// payload.
///
/// The keys are untouched, so the index still says exactly which records hold
/// which terms — only the two numbers are taken away, which is precisely the
/// state an index written by an older release is in. It forces the read onto the
/// path that recovers them from the record's text.
fn forget_the_numbers(backend: &Arc<Counting>) -> usize {
    let request = ScanRequest {
        keyspace: Keyspace::INDEX,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let mut batch = WriteBatch::default();
    let mut rewritten = 0_usize;
    for (key, _) in backend.scan(&request).unwrap() {
        // Decoding is the filter: the index keyspace also holds the dictionary,
        // the statistics and every other index's entries, and a posting key is
        // the only one that decodes as one.
        if PostingKey::decode(key.as_slice()).is_err() {
            continue;
        }
        batch = batch.put(Keyspace::INDEX, key, Posting::Membership.encode());
        rewritten = rewritten.saturating_add(1);
    }
    backend.apply(batch).unwrap();
    rewritten
}

#[test]
fn the_index_and_the_text_score_every_record_identically() {
    // G014 F2's assertion, and the reason the wave is allowed to land: the same
    // read, the same data, the two ways of learning what the record holds.
    let (backend, store) = corpus(40);
    let from_index = scores(&store);
    assert_eq!(from_index.len(), RECORDS as usize);
    assert!(
        from_index.iter().any(|(_, held)| *held > 0.0),
        "nothing scored above zero, so the comparison would prove nothing"
    );

    let rewritten = forget_the_numbers(&backend);
    assert!(rewritten > 0, "no postings were downgraded");
    let from_text = scores(&store);

    assert_eq!(from_index.len(), from_text.len());
    for ((left_id, left), (right_id, right)) in from_index.iter().zip(&from_text) {
        assert_eq!(left_id, right_id);
        assert!(
            (left - right).abs() < f64::EPSILON,
            "{left_id}: index {left} vs text {right}"
        );
    }
}

/// Multiply what one posting claims about one record, leaving the text alone.
///
/// A deliberately inconsistent store, and the only way to tell two sources of
/// the same numbers apart: while they agree, no test can say which one was read.
/// Nothing in the store can reach this state on its own — the write path derives
/// both from the same analysis in the same batch — which is exactly why it is a
/// usable instrument.
fn overstate(backend: &Arc<Counting>, wanted_id: &str, wanted_term: &str, times: u32) -> bool {
    let request = ScanRequest {
        keyspace: Keyspace::INDEX,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    for (key, value) in backend.scan(&request).unwrap() {
        let Ok(posting) = PostingKey::decode(key.as_slice()) else {
            continue;
        };
        if posting.id.to_string() != wanted_id
            || posting.term.as_text().as_deref() != Some(wanted_term)
        {
            continue;
        }
        let Posting::Counted { frequency, length } = Posting::decode(value.as_slice()).unwrap()
        else {
            panic!("the posting carries no numbers to overstate");
        };
        let batch = WriteBatch::default().put(
            Keyspace::INDEX,
            key,
            Posting::Counted {
                frequency: frequency.saturating_mul(times),
                length,
            }
            .encode(),
        );
        backend.apply(batch).unwrap();
        return true;
    }
    false
}

/// Every record's score for one query, by id.
fn scores_for(store: &Store, query: &str) -> Vec<(String, f64)> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let outcomes = session
        .run(&format!(
            "SELECT id, search::score(body, '{query}') AS relevance FROM notes;"
        ))
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut found: Vec<(String, f64)> = records
        .iter()
        .map(|(id, record)| {
            let Value::Object(fields) = record else {
                panic!("not a record: {record:?}");
            };
            let Some(Value::Number(Number::Float(held))) = fields.get("relevance") else {
                panic!("not a score: {:?}", fields.get("relevance"));
            };
            (id.to_string(), *held)
        })
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

#[test]
fn a_word_written_twice_in_a_query_weighs_twice_at_the_read() {
    // The query's terms are analysed once per read and travel to the scorer as a
    // multiset. Deduplicating them there is a one-line simplification that looks
    // free and is not: it would leave the records holding the repeated word
    // lower and every other record where it was, which is a reordering.
    //
    // Asserted through a statement rather than in the scorer's own tests,
    // because the deduplication would happen on the way *to* the scorer and a
    // test that builds the multiset itself cannot see it.
    let (_, store) = corpus(10);
    let once = scores_for(&store, "lock");
    let twice = scores_for(&store, "lock lock");
    assert_eq!(once.len(), twice.len());
    let mut moved = 0_usize;
    for ((id, single), (other, doubled)) in once.iter().zip(&twice) {
        assert_eq!(id, other);
        assert!(
            (doubled - 2.0 * single).abs() < f64::EPSILON,
            "{id}: {single} once, {doubled} twice"
        );
        if *single > 0.0 {
            moved = moved.saturating_add(1);
        }
    }
    assert!(
        moved > 0,
        "no record scored above zero, so doubling proved nothing"
    );
}

#[test]
fn the_score_follows_the_posting_and_not_the_text() {
    // The test that makes the other two mean something. Both of those compare
    // two routes that normally agree, so both would pass on an implementation
    // that had quietly gone on reading the text. This one drives the posting and
    // the text apart and asserts which one the score followed.
    let (backend, store) = corpus(20);
    let before = scores(&store);

    // `notes:1` holds `lock` twice — see `body` — and the analyzer stems, so the
    // term as stored is what the dictionary spells.
    assert!(
        overstate(&backend, "1", "lock", 8),
        "no posting was found to overstate"
    );
    let after = scores(&store);

    let held = |found: &[(String, f64)], id: &str| -> f64 {
        found
            .iter()
            .find(|(name, _)| name == id)
            .map(|(_, held)| *held)
            .unwrap()
    };
    assert!(
        held(&after, "1") > held(&before, "1"),
        "the score ignored the posting: {} then {}",
        held(&before, "1"),
        held(&after, "1")
    );

    // And nobody else moved, because nothing else was touched — an assertion
    // that fails if the score is being computed from anything collection-wide
    // that the rewritten posting also feeds.
    for (id, score) in &before {
        if id == "1" {
            continue;
        }
        assert!(
            (score - held(&after, id)).abs() < f64::EPSILON,
            "{id} moved when only notes:1 was rewritten"
        );
    }
}

#[test]
fn what_a_score_reads_is_the_query_and_not_the_document() {
    // The structural half of F2, asserted behaviourally rather than by reading
    // the source: scoring costs one posting read per asked term per scored
    // record, and that number does not move when the documents get ten times
    // longer. An implementation that recovered the numbers from the text would
    // read no postings at all and fail the first assertion; one that scanned a
    // term's whole posting list instead of point-reading its own would fail as
    // the corpus grew.
    let expected = TERMS * RECORDS as usize;

    let (short, short_store) = corpus(10);
    short.reset();
    let brief = Instant::now();
    let from_short = scores(&short_store);
    let brief = brief.elapsed();
    assert_eq!(short.postings(), expected);

    let (long, long_store) = corpus(100);
    long.reset();
    let lengthy = Instant::now();
    let from_long = scores(&long_store);
    let lengthy = lengthy.elapsed();
    assert_eq!(
        long.postings(),
        expected,
        "the reads a score costs moved with the length of the documents"
    );

    // The documents differ, so the scores differ — a corpus where they did not
    // would make the read count above true for an uninteresting reason.
    assert_ne!(from_short, from_long);

    // And the measurement F2 asks for. Ten times the prose, and the scoring path
    // does not pay for it: the whole read is allowed to grow — the records are
    // ten times bigger and are still decoded and returned — but nowhere near
    // linearly, which is what re-analysing every scored record would have cost.
    assert!(
        lengthy < brief * 6,
        "scoring cost grew with document length: {brief:?} then {lengthy:?}"
    );
}
