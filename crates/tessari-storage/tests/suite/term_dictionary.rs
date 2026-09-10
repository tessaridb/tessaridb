//! The term dictionary and the postings agree, in both directions.
//!
//! A dictionary entry is a claim about the postings beside it: *this many
//! records hold this word*. Nothing checks that claim at read time — unlike an
//! ordinary index entry, which is confirmed against the record before it reaches
//! an answer — because the number is not a candidate, it is an input to a score.
//! A wrong one does not produce a wrong row; it produces a **plausible wrong
//! order**, which is the failure this store refuses everywhere it can.
//!
//! So the two directions fail differently and both are swept:
//!
//! - **Posting → entry.** A term with postings and no dictionary entry falls
//!   through to the counting path and is still ranked correctly, but it is
//!   invisible to a prefix walk — so the word exists, matches, and cannot be
//!   completed or suggested. Silent by construction.
//! - **Entry → posting.** An entry whose count exceeds the postings weighs the
//!   term as commoner than it is, and every score taken against it is wrong by
//!   an amount nobody can see. An entry left behind at zero is worse than wrong:
//!   it is a word a prefix walk offers and whose posting list is empty.
//!
//! The expected counts are derived here from the postings, which is a second and
//! independent statement of what the entries should be. Comparing the store's
//! maintenance against itself would prove nothing.
//!
//! The workload is pseudo-random and **deterministic** — fixed seed, a
//! multiplicative generator — so a failure is reproducible from the seed rather
//! than being a story about a run nobody can repeat.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tessari_encoding::{
    IndexAddress, IndexValues, KeyKind, Posting, PostingKey, SearchTermKey, StoreKey, StoreValue,
    TermStatistics, encode_payload,
};
use tessari_kv::{Key, KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_storage::{
    Catalog, FieldShape, IndexDefinition, IndexShape, RecordAddress, Store, TableShape,
};
use tessari_types::{
    Analyzer, DatabaseId, FieldKind, Filter, NamespaceId, Path, RecordId, TableId, Value,
};

/// The seed the workload runs from. Printed by every failing assertion.
const SEED: u64 = 0x7e12_d1c7_1047_2026;
/// How many workload steps to run.
const STEPS: u64 = 300;
/// How many distinct records the workload writes over.
const RECORDS: u64 = 30;

/// The words the workload draws from.
///
/// Small and overlapping on purpose. A vocabulary large enough that every record
/// holds unique words would never exercise the case the dictionary exists for —
/// one term held by many records, whose count moves as they come and go — and
/// the shared prefixes are what a later prefix walk will be bounded by.
const WORDS: &[&str] = &[
    "vector",
    "vectors",
    "vectorised",
    "lock",
    "locks",
    "contention",
    "index",
    "indexes",
    "indexing",
    "term",
];

/// A multiplicative congruential generator, so the workload is a function of the
/// seed and nothing else.
struct Rolls(u64);

impl Rolls {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }
}

struct Fixture {
    backend: Arc<dyn KvBackend>,
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    index: IndexDefinition,
}

impl Fixture {
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(Arc::clone(&backend)).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap();
        let database = catalog.create_database(namespace.id, "shop").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "notes", TableShape::default())
            .unwrap();
        // The analyzer is on the **field**. A search index over a field that
        // declares none posts no terms at all, silently — so a sweep that found
        // an empty dictionary matching empty postings would pass while proving
        // nothing. The population assertion in each test is what catches that.
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
        Self {
            backend,
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            index,
        }
    }

    fn address(&self) -> IndexAddress {
        IndexAddress::new(
            self.index.namespace,
            self.index.database,
            self.index.table,
            self.index.id,
        )
    }

    fn at(&self, n: u64) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(format!("n{n:03}")),
        )
    }

    fn write(&self, n: u64, body: &str) {
        let mut transaction = self.store.begin().unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("body".to_owned(), Value::from(body));
        transaction.put(
            self.at(n),
            encode_payload(&Value::Object(fields)).into_bytes(),
        );
        transaction.commit().unwrap();
    }

    fn delete(&self, n: u64) {
        let mut transaction = self.store.begin().unwrap();
        transaction.delete(self.at(n));
        transaction.commit().unwrap();
    }

    fn keys(&self, kind: KeyKind) -> Vec<Vec<u8>> {
        let request = ScanRequest {
            keyspace: kind.keyspace(),
            range: KeyRange::prefix(&self.address().prefix(kind)),
            direction: ScanDirection::Forward,
            limit: None,
        };
        self.backend
            .scan(&request)
            .unwrap()
            .into_iter()
            .map(|(key, _)| key.as_slice().to_vec())
            .collect()
    }

    /// How many records hold each term, derived from the **postings**.
    ///
    /// The independent statement the dictionary is checked against.
    fn counted_from_postings(&self) -> BTreeMap<IndexValues, u64> {
        let mut found: BTreeMap<IndexValues, u64> = BTreeMap::new();
        for key in self.keys(KeyKind::Posting) {
            let posting = PostingKey::decode(&key).unwrap();
            let held = found.entry(posting.term).or_default();
            *held = held.saturating_add(1);
        }
        found
    }

    /// What the dictionary says, term by term.
    fn dictionary(&self) -> BTreeMap<IndexValues, u64> {
        let request = ScanRequest {
            keyspace: KeyKind::SearchTerm.keyspace(),
            range: KeyRange::prefix(&self.address().prefix(KeyKind::SearchTerm)),
            direction: ScanDirection::Forward,
            limit: None,
        };
        self.backend
            .scan(&request)
            .unwrap()
            .into_iter()
            .map(|(key, value)| {
                let term = SearchTermKey::decode(key.as_slice()).unwrap().term;
                let held = TermStatistics::decode(value.as_slice()).unwrap();
                (term, held.documents)
            })
            .collect()
    }

    /// The extremes each term's **postings** actually exhibit.
    ///
    /// The independent statement the stored bound is checked against, derived
    /// the same way `counted_from_postings` derives the count: from the postings
    /// themselves, never from the entry that claims to summarise them.
    fn extremes_from_postings(&self) -> BTreeMap<IndexValues, (u32, u32)> {
        let request = ScanRequest {
            keyspace: KeyKind::Posting.keyspace(),
            range: KeyRange::prefix(&self.address().prefix(KeyKind::Posting)),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut found: BTreeMap<IndexValues, (u32, u32)> = BTreeMap::new();
        for (key, value) in self.backend.scan(&request).unwrap() {
            let term = PostingKey::decode(key.as_slice()).unwrap().term;
            let Posting::Counted { frequency, length } = Posting::decode(value.as_slice()).unwrap()
            else {
                continue;
            };
            found
                .entry(term)
                .and_modify(|(most, fewest)| {
                    *most = (*most).max(frequency);
                    *fewest = (*fewest).min(length);
                })
                .or_insert((frequency, length));
        }
        found
    }

    /// What the dictionary claims each term's extremes are.
    fn bounds(&self) -> BTreeMap<IndexValues, TermStatistics> {
        let request = ScanRequest {
            keyspace: KeyKind::SearchTerm.keyspace(),
            range: KeyRange::prefix(&self.address().prefix(KeyKind::SearchTerm)),
            direction: ScanDirection::Forward,
            limit: None,
        };
        self.backend
            .scan(&request)
            .unwrap()
            .into_iter()
            .map(|(key, value)| {
                let term = SearchTermKey::decode(key.as_slice()).unwrap().term;
                (term, TermStatistics::decode(value.as_slice()).unwrap())
            })
            .collect()
    }

    /// The bound covers every posting it is a bound over.
    ///
    /// **This is the soundness assertion and it is one-sided on purpose.** The
    /// stored numbers are allowed to be looser than the postings warrant —
    /// neither is relaxed when a posting leaves, because an extreme cannot move
    /// inward without knowing the second one — so the test asserts domination
    /// and never equality. Asserting equality would fail on correct behaviour;
    /// asserting nothing would miss the only failure that matters.
    ///
    /// The direction is the whole point (ADR-0050). A bound too loose costs
    /// pruning efficiency. A bound too tight removes records from an answer and
    /// leaves nothing in an error state.
    fn assert_bounds_dominate(&self, after: &str) {
        let observed = self.extremes_from_postings();
        for (term, held) in self.bounds() {
            let Some((most, fewest)) = observed.get(&term) else {
                continue;
            };
            let Some((frequency, length)) = held.bound() else {
                panic!("{after}: a term with postings has no bound at all (seed {SEED:#x})");
            };
            assert!(
                frequency >= *most,
                "{after}: a bound's frequency {frequency} sits below a posting's {most} \
                 — records this term would win are pruned away (seed {SEED:#x})"
            );
            assert!(
                length <= *fewest,
                "{after}: a bound's length {length} sits above a posting's {fewest} \
                 — a shorter record scores higher than the bound allows (seed {SEED:#x})"
            );
        }
    }

    /// Both directions at once, with the failing term named.
    fn assert_agrees(&self, after: &str) {
        self.assert_bounds_dominate(after);
        let expected = self.counted_from_postings();
        let held = self.dictionary();
        for (term, count) in &expected {
            assert_eq!(
                held.get(term),
                Some(count),
                "{after}: a term with {count} postings has no matching entry (seed {SEED:#x})"
            );
        }
        for (term, count) in &held {
            assert!(
                *count > 0,
                "{after}: an entry survives at zero (seed {SEED:#x})"
            );
            assert_eq!(
                expected.get(term),
                Some(count),
                "{after}: an entry counts records the postings do not (seed {SEED:#x})"
            );
        }
        assert_eq!(
            held.len(),
            expected.len(),
            "{after}: the two vocabularies differ in size (seed {SEED:#x})"
        );
    }
}

/// A body of one to three words drawn from the vocabulary, sometimes repeating a
/// word — so a term's *frequency* in one record and its *document* count are
/// different numbers, which is the confusion the dictionary could quietly make.
fn body(rolls: &mut Rolls) -> String {
    let words = rolls.below(3).saturating_add(1);
    (0..words)
        .map(|_| {
            let at = usize::try_from(rolls.below(u64::try_from(WORDS.len()).unwrap())).unwrap();
            WORDS[at]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_dictionary_and_the_postings_agree_after_an_arbitrary_workload() {
    let fixture = Fixture::new();
    let mut rolls = Rolls(SEED);
    for _ in 0..STEPS {
        let which = rolls.below(RECORDS);
        // Deletes are a third of the workload rather than a rare case: a term's
        // count falling is the direction that needs a stored second-largest to
        // get wrong, and the direction a maintenance that only counted arrivals
        // would pass every test but this one.
        if rolls.below(3) == 0 {
            fixture.delete(which);
        } else {
            let text = body(&mut rolls);
            fixture.write(which, &text);
        }
    }
    // The sweep proves nothing over an empty index.
    assert!(
        !fixture.dictionary().is_empty(),
        "the workload posted no terms — the field's analyzer is not attached"
    );
    fixture.assert_agrees("after the workload");
}

#[test]
fn a_word_no_record_holds_any_more_leaves_the_dictionary() {
    let fixture = Fixture::new();
    fixture.write(1, "vector contention");
    fixture.write(2, "vector index");
    let held = fixture.dictionary();
    assert_eq!(held.len(), 3, "{held:?}");

    // The record holding the only occurrence of `contention` is rewritten
    // without it. The word must leave: a dictionary that kept it would offer a
    // completion whose posting list is empty.
    fixture.write(1, "vector index");
    let held = fixture.dictionary();
    assert_eq!(held.len(), 2, "{held:?}");
    fixture.assert_agrees("after a word was written out of the last record holding it");

    // And deleting both records empties the dictionary rather than leaving a
    // vocabulary behind.
    fixture.delete(1);
    fixture.delete(2);
    assert!(
        fixture.dictionary().is_empty(),
        "{:?}",
        fixture.dictionary()
    );
    assert!(fixture.keys(KeyKind::Posting).is_empty());
}

#[test]
fn a_term_held_by_several_records_counts_them_and_not_its_occurrences() {
    let fixture = Fixture::new();
    // Three records, and the first holds the word three times. The document
    // frequency is three — the number of *records* — and a maintenance that
    // accumulated the posting's frequency instead would say five.
    fixture.write(1, "lock lock lock");
    fixture.write(2, "lock contention");
    fixture.write(3, "lock index");
    let transaction = fixture.store.begin().unwrap();
    assert_eq!(
        transaction
            .document_frequency(&fixture.index, "lock")
            .unwrap(),
        3
    );
    assert_eq!(
        transaction
            .document_frequency(&fixture.index, "contention")
            .unwrap(),
        1
    );
    // A word nobody holds is zero rather than an error or an absence.
    assert_eq!(
        transaction
            .document_frequency(&fixture.index, "unheardof")
            .unwrap(),
        0
    );
    fixture.assert_agrees("after three records");
}

#[test]
fn a_rebuild_makes_the_dictionary_what_the_rows_imply_rather_than_adding_to_it() {
    let fixture = Fixture::new();
    for n in 0..8 {
        fixture.write(n, "vector index term");
    }
    fixture.assert_agrees("before the rebuild");

    // `REBUILD INDEX` writes the index's catalog record again, unchanged. A
    // rebuild that added to the stored figure would double every count — and
    // nothing would raise, because a document frequency has no reader who would
    // notice it drifting.
    let mut transaction = fixture.store.begin().unwrap();
    Catalog::new(&mut transaction).rebuild_index(&fixture.index);
    transaction.commit().unwrap();

    fixture.assert_agrees("after the rebuild");
    let held = fixture.dictionary();
    assert_eq!(held.len(), 3, "{held:?}");
    for (term, count) in &held {
        assert_eq!(*count, 8, "{term:?} counted {count}");
    }
}

#[test]
fn an_index_with_no_dictionary_still_answers_the_frequency_it_always_did() {
    // What an index written before the dictionary existed looks like: postings,
    // no entries. It must keep ranking at the old cost rather than reporting
    // every term as held by nobody, which would make a common word the rarest
    // thing in the collection and invert the ranking.
    let fixture = Fixture::new();
    fixture.write(1, "lock contention");
    fixture.write(2, "lock index");

    let mut batch = WriteBatch::default();
    for key in fixture.keys(KeyKind::SearchTerm) {
        batch = batch.delete(KeyKind::SearchTerm.keyspace(), Key::from(key));
    }
    fixture.backend.apply(batch).unwrap();
    assert!(fixture.dictionary().is_empty());

    let transaction = fixture.store.begin().unwrap();
    assert_eq!(
        transaction
            .document_frequency(&fixture.index, "lock")
            .unwrap(),
        2
    );
    assert_eq!(
        transaction
            .document_frequency(&fixture.index, "index")
            .unwrap(),
        1
    );
}

#[test]
fn dropping_the_index_leaves_no_dictionary_behind() {
    let fixture = Fixture::new();
    fixture.write(1, "vector index");
    assert!(!fixture.dictionary().is_empty());

    // The entries a rebuild clears are the entries a drop must not leave: a
    // dictionary outliving its index is a vocabulary nothing will ever
    // reconcile, in a keyspace nothing scans on its own.
    let terms: BTreeSet<Vec<u8>> = fixture.keys(KeyKind::SearchTerm).into_iter().collect();
    assert!(!terms.is_empty());
    let mut transaction = fixture.store.begin().unwrap();
    Catalog::new(&mut transaction)
        .drop_index(fixture.index.id)
        .unwrap();
    transaction.commit().unwrap();
    // A dropped index keeps its entries by design — the same stance a dropped
    // table takes towards its records — so this asserts what the store actually
    // promises rather than what a reader might assume, and pins it so a change
    // of that stance is a decision rather than a surprise.
    assert_eq!(
        fixture
            .keys(KeyKind::SearchTerm)
            .into_iter()
            .collect::<BTreeSet<_>>(),
        terms,
        "the drop moved the dictionary without moving the postings"
    );
    assert!(!fixture.keys(KeyKind::Posting).is_empty());
}

/// A bound goes **loose** when a posting leaves, never wrong.
///
/// The deciding case for ADR-0050's maintenance rule, and the workload sweep
/// above cannot distinguish it: that sweep asserts domination, which a bound
/// that *did* fall back would still satisfy on most steps. Here the record
/// carrying the extreme is deleted and the bound is asserted to have stayed
/// where it was.
///
/// It stays because an extreme cannot move inward without knowing the second
/// one, and finding that means reading the term's whole posting range on every
/// delete — an O(df) scan on the write path to buy a tighter bound on the read
/// path. The trade is deliberate and its cost is looseness, which a rebuild
/// repairs.
#[test]
fn a_bound_survives_the_posting_that_set_it_leaving() {
    let fixture = Fixture::new();
    // One long record holding the word many times sets both extremes; one short
    // record holding it once would set them very differently.
    fixture.write(
        0,
        "vector vector vector vector index term graph walk edge node",
    );
    fixture.write(1, "vector index");
    fixture.assert_agrees("with both records");

    let word = IndexValues::of(&[Value::from("vector")]);
    let before = fixture.bounds().get(&word).copied().unwrap();
    assert_eq!(
        before.bound(),
        Some((4, 2)),
        "the extremes are not the ones the two records imply"
    );

    fixture.delete(0);
    fixture.assert_agrees("after the extreme's record left");

    let after = fixture.bounds().get(&word).copied().unwrap();
    assert_eq!(
        after.documents, 1,
        "the count did not follow the record out"
    );
    assert_eq!(
        after.bound(),
        Some((4, 2)),
        "the bound tightened on a delete, which it cannot do soundly without \
         knowing the second-largest posting"
    );
}

/// A rebuild is where a loose bound is repaired.
///
/// This is the other half of the trade above: maintenance never tightens, so
/// something has to, and it is the operation that already recomputes the count
/// from the rows rather than adding to it. Without this the looseness is
/// permanent and the pruning path decays for the life of the store.
#[test]
fn a_rebuild_recomputes_a_bound_that_maintenance_left_loose() {
    let fixture = Fixture::new();
    fixture.write(
        0,
        "vector vector vector vector index term graph walk edge node",
    );
    fixture.write(1, "vector index");
    fixture.delete(0);

    let word = IndexValues::of(&[Value::from("vector")]);
    assert_eq!(
        fixture.bounds().get(&word).unwrap().bound(),
        Some((4, 2)),
        "the loose bound this test exists to repair was not there to begin with"
    );

    let mut transaction = fixture.store.begin().unwrap();
    Catalog::new(&mut transaction).rebuild_index(&fixture.index);
    transaction.commit().unwrap();

    fixture.assert_agrees("after the rebuild");
    assert_eq!(
        fixture.bounds().get(&word).unwrap().bound(),
        Some((1, 2)),
        "the rebuild left the departed record's extreme in place"
    );
}

/// A rewrite that keeps a word still moves its bound.
///
/// The case a count-only dictionary is blind to. Rewriting a record that holds
/// the word nets a document-count delta of **zero**, so a settle keyed on the
/// delta alone skips the term entirely — and the word may now occur twice as
/// often, in a record half as long. The bound would then sit below a posting
/// that exists, which is the one direction ADR-0050 forbids.
#[test]
fn a_rewrite_that_changes_no_count_still_raises_the_bound() {
    let fixture = Fixture::new();
    fixture.write(0, "vector index term graph");
    let word = IndexValues::of(&[Value::from("vector")]);
    assert_eq!(fixture.bounds().get(&word).unwrap().bound(), Some((1, 4)));

    // Same record, same word, still exactly one document holding it.
    fixture.write(0, "vector vector vector");
    assert_eq!(
        fixture.bounds().get(&word).unwrap().documents,
        1,
        "the rewrite changed the count, so this is not the case under test"
    );
    assert_eq!(
        fixture.bounds().get(&word).unwrap().bound(),
        Some((3, 3)),
        "a rewrite that left the count alone left the bound behind with it"
    );
    fixture.assert_agrees("after the rewrite");
}

/// An entry written before the bound existed reads as *no bound*, not as zero.
///
/// The difference decides whether an older index is readable. A zero pair read
/// as a bound is an upper bound of zero — every term contributes nothing, so
/// every term is prunable, so a pruning read over an index written by an earlier
/// build answers with nothing at all. Read as *no bound* it obliges the caller to
/// score the postings, which is exactly what that build already did.
#[test]
fn an_entry_from_before_the_bound_says_it_has_none() {
    // The shorter payload an earlier build wrote: the count and nothing after it.
    let older = TermStatistics::new(12).encode();
    let read = TermStatistics::decode(older.as_slice()).unwrap();
    assert_eq!(
        read.documents, 12,
        "the count did not survive the shorter form"
    );
    assert_eq!(
        read.bound(),
        None,
        "an entry with no extremes offered a bound of zero, which prunes everything"
    );

    // And a current entry that genuinely has extremes is not confused with it.
    let current = TermStatistics::bounded(12, 3, 40).encode();
    assert_eq!(
        TermStatistics::decode(current.as_slice()).unwrap().bound(),
        Some((3, 40))
    );
}
