//! How well a record answers a query, measured against a collection.
//!
//! # A score is not a property of a document
//!
//! `MATCHES` asks whether *this* text holds *these* words, and the answer is a
//! property of the record alone — which is why a scan and an index give the same
//! one, asserted record for record. A score is a different kind of question. It
//! asks how much a word occurring here is worth, and a word is worth something
//! only relative to how often it occurs everywhere else.
//!
//! So a score needs four numbers, and only two of them can be read off the
//! record: how often the term occurs in it, and how long it is. The other two —
//! how many documents there are and how long a typical one is — are properties
//! of the collection, and are maintained beside the postings that summarise it
//! (`tessari_encoding::SearchStatistics`).
//!
//! # Which is why a score without an index is refused rather than answered
//!
//! At first reading this breaks the rule the whole store is built on: which
//! access path runs is decided by what exists, and the answer never is. It does
//! not, and the distinction is worth stating because the alternative is worse in
//! a way that is hard to see.
//!
//! Without an index there are no collection statistics, so the honest options
//! are to refuse, or to answer with something. Answering `0` for every record —
//! or scoring against whatever subset happened to be read — produces an ordering
//! that looks exactly like a ranking and is not one. That is the mistake the
//! vector wave caught from the other direction, where an absent distance sorted
//! *first* and records with no embedding at all "looked exactly like results".
//!
//! A refusal naming the field is a statement that did not run. A plausible wrong
//! order is a statement that did, and nobody checks it.
//!
//! # BM25, and the one deviation from the textbook
//!
//! ```text
//! idf(t) = ln(1 + (N - df + 0.5) / (df + 0.5))
//! score  = Σ  idf(t) · f·(k1+1) / (f + k1·(1 - b + b·len/avg))
//! ```
//!
//! The `1 +` inside the logarithm is not decoration. Textbook BM25 uses
//! `ln((N - df + 0.5) / (df + 0.5))`, which turns **negative** once a term is in
//! more than half the collection — so a document containing a common query word
//! would score below one that does not contain it at all. Adding one to the
//! argument keeps every term's contribution non-negative, which is the behaviour
//! anyone reading a result list assumes.
//!
//! `k1` controls how quickly repetition stops helping, `b` how much length is
//! held against a document. Both are constants here rather than options on the
//! index definition: no measurement in this project justifies any other value,
//! and a knob nobody can yet turn responsibly is one more thing to get wrong.
//! The cost of that is stated in the specification — changing them in a release
//! changes ranking order without any statement changing.

use std::collections::BTreeMap;

use tessari_constants::{BM25_B, BM25_K1};
use tessari_encoding::TermStatistics;
use tessari_types::{Analyzer, Number, Value};

mod explain;

pub(crate) use explain::explain;

/// What one searched field's collection looks like, resolved once per read.
///
/// The `terms` map holds only the terms the statement actually asks about —
/// resolving them all would be reading the index to answer a question nobody
/// put.
#[derive(Debug, Clone)]
pub(crate) struct Corpus {
    /// How many records hold at least one term.
    pub(crate) documents: u64,
    /// The length of a typical document, in tokens.
    pub(crate) average_length: f64,
    /// What the dictionary says about each term the statement names.
    ///
    /// The whole entry rather than the frequency alone, because the same point
    /// read that counts a term also carries the extremes [`bound`] scores an
    /// upper bound from, and reading it twice would spend the saving the
    /// dictionary exists for.
    pub(crate) terms: BTreeMap<String, TermStatistics>,
    /// The query's terms, **with repeats**, analysed once per read.
    ///
    /// A multiset rather than the `terms` keys, which are deduplicated. A
    /// word written twice in a query weighs twice today, and it weighs twice for
    /// the records that hold it and not at all for the ones that do not — so
    /// deduplicating here would not rescale the scores, it would reorder them.
    pub(crate) asked: Vec<String>,
    /// Each starred word of the query, weighed as one term (ADR-0104).
    pub(crate) blends: Vec<Blend>,
}

/// A starred word, scored as **one** term over the terms it begins.
///
/// The blended-frequency rewrite: every expansion shares the largest document
/// frequency among them, and a record's frequency is the sum of its occurrences
/// of all of them. Weighing each by its own frequency instead would rank a rare
/// misspelling under the prefix above the common word the reader was typing,
/// because the rarer a term the more each occurrence of it is worth.
#[derive(Debug, Clone)]
pub(crate) struct Blend {
    /// The prefix as it was typed.
    pub(crate) prefix: String,
    /// The terms it reaches, at most the expansion cap of them, the most-held
    /// first.
    pub(crate) expansions: Vec<String>,
    /// What one occurrence of each expansion counts for, in the same order:
    /// `1` for a prefix's terms, `1 / (1 + edits)` for a fuzzy word's, so an
    /// exact term always outweighs a corrected one (G058 C3, Q-909).
    pub(crate) weights: Vec<f64>,
    /// The largest document frequency among them.
    pub(crate) documents: u64,
}

impl Corpus {
    /// Every term a record's occurrences are counted for: the asked ones and
    /// every expansion of a starred word.
    pub(crate) fn counted(&self) -> Vec<String> {
        let mut terms = self.asked.clone();
        for blend in &self.blends {
            terms.extend(blend.expansions.iter().cloned());
        }
        terms
    }
}

/// What one record holds of a query's terms, and how long it is.
///
/// The two numbers BM25 needs about the document being scored, as opposed to the
/// two in [`Corpus`] that describe the collection it is scored against.
#[derive(Debug, Clone, Default)]
pub(crate) struct Held {
    /// Occurrences of each asked term in this record, **with** repeats. A term
    /// the record does not hold is absent rather than present at zero.
    occurrences: BTreeMap<String, u32>,
    /// Tokens in the record's analysed field, **with** repeats.
    length: u32,
}

impl Held {
    /// What the index says, read from the postings.
    pub(crate) const fn counted(occurrences: BTreeMap<String, u32>, length: u32) -> Self {
        Self {
            occurrences,
            length,
        }
    }

    /// What the record's own text says, analysed here.
    ///
    /// The path an index written before postings carried a payload still takes,
    /// and the only place on the scoring path that analyses anything. It reaches
    /// the same two numbers the writer would have stored — `terms_of` counts the
    /// same way — which is why the two paths can be asserted equal per record.
    pub(crate) fn analysed(analyzer: &Analyzer, text: &str, asked: &[String]) -> Self {
        let terms = analyzer.terms(text);
        let mut occurrences = BTreeMap::new();
        for term in asked {
            if occurrences.contains_key(term) {
                continue;
            }
            let counted = terms.iter().filter(|held| *held == term).count();
            if counted > 0 {
                occurrences.insert(term.clone(), count(counted));
            }
        }
        Self {
            occurrences,
            length: count(terms.len()),
        }
    }
}

/// The BM25 score of what one record holds against one query.
///
/// The record's own two numbers arrive already resolved, because where they come
/// from is the caller's decision and not the ranking's: from the postings for an
/// index that carries them, from the text for one written before they existed.
/// Either way the words being counted are the words the index posted — the rule
/// SGC.T1 exists for, applied one layer up.
pub(crate) fn score(corpus: &Corpus, held: &Held) -> Value {
    Value::Number(Number::float(scored(corpus, held)))
}

/// The same number before it becomes a value.
///
/// A pruning walk compares scores against each other rather than returning them,
/// and going through [`Value`] to do that would put a number's *representation*
/// in the way of a comparison the walk's correctness rests on.
///
/// Nothing scores `0`, and deliberately not `NONE`: a document holding none of
/// the query's words scores zero, which is a computed answer rather than an
/// absence standing in for one. It also sorts where it belongs under the `DESC`
/// a ranked read is written with.
pub(crate) fn scored(corpus: &Corpus, held: &Held) -> f64 {
    if (corpus.asked.is_empty() && corpus.blends.is_empty()) || corpus.documents == 0 {
        return 0.0;
    }
    let Some(average) = positive(corpus.average_length) else {
        return 0.0;
    };

    let length = as_float(u64::from(held.length));
    let total = as_float(corpus.documents);

    let mut sum = 0.0_f64;
    for term in &corpus.asked {
        // A term the record does not hold contributes nothing, however rare it
        // is — so a query word that is in no document at all cannot lift every
        // score by the same amount and change nothing but the numbers.
        let Some(occurrences) = held.occurrences.get(term) else {
            continue;
        };
        let occurrences = as_float(u64::from(*occurrences));
        let frequency = as_float(corpus.terms.get(term).map_or(0, |held| held.documents));
        sum +=
            inverse_document_frequency(total, frequency) * saturation(occurrences, length, average);
    }
    for blend in &corpus.blends {
        let occurrences: f64 = blend
            .expansions
            .iter()
            .zip(&blend.weights)
            .filter_map(|(term, weight)| {
                held.occurrences
                    .get(term)
                    .map(|one| as_float(u64::from(*one)) * weight)
            })
            .sum();
        if occurrences <= 0.0 {
            continue;
        }
        sum += inverse_document_frequency(total, as_float(blend.documents))
            * saturation(occurrences, length, average);
    }
    sum
}

/// The most this term can contribute to any one record's score.
///
/// `None` is **this term cannot be bounded**, which obliges a caller to score
/// its postings rather than prune them. Three things reach it: an entry written
/// before the extremes existed, a term nothing holds, and a collection with no
/// documents or no length to normalise against. All three mean the same thing to
/// a pruning read, and none of them mean a bound of zero — a term that can
/// contribute nothing is precisely a term worth pruning, which is the opposite
/// answer.
///
/// # Why the bound is computed here and not stored
///
/// A contribution is `idf × saturation(occurrences, length, average_length)`,
/// and `average_length` belongs to the **collection**: it moves on every write,
/// and when it grows the same posting scores higher. A stored impact is
/// therefore an under-estimate of what its own postings score today, and an
/// under-estimated upper bound is not loose but unsound — the term is pruned and
/// the records it would have won go missing with nothing in an error state
/// (measured at up to 34× in ADR-0050's framing).
///
/// So the entry stores only what belongs to the postings — the most occurrences
/// any one of them records, and the fewest tokens any of their records holds —
/// and the formula is evaluated where the collection's numbers are already in
/// hand. That dominates every posting at **any** average, because `saturation`
/// is increasing in occurrences and decreasing in length, so pairing the largest
/// frequency with the shortest length can only over-estimate. The two usually
/// come from different records, which is what makes the bound loose; loose costs
/// pruning efficiency, and the other direction costs records.
pub(crate) fn bound(corpus: &Corpus, term: &str) -> Option<f64> {
    let held = corpus.terms.get(term)?;
    let (max_frequency, min_length) = held.bound()?;
    if corpus.documents == 0 {
        return None;
    }
    let average = positive(corpus.average_length)?;
    let weight = inverse_document_frequency(as_float(corpus.documents), as_float(held.documents));
    Some(
        weight
            * saturation(
                as_float(u64::from(max_frequency)),
                as_float(u64::from(min_length)),
                average,
            ),
    )
}

/// How much one occurrence of this term is worth.
fn inverse_document_frequency(documents: f64, holding: f64) -> f64 {
    let numerator = documents - holding + 0.5;
    let denominator = holding + 0.5;
    // See the module documentation: without the `1 +` a term held by more than
    // half the collection would carry a *negative* weight.
    (1.0 + numerator / denominator).ln()
}

/// How much the term's repetition counts for, given how long the document is.
fn saturation(occurrences: f64, length: f64, average: f64) -> f64 {
    let normalised = BM25_K1.mul_add(1.0 - BM25_B + BM25_B * (length / average), occurrences);
    occurrences * (BM25_K1 + 1.0) / normalised
}

/// A count of tokens, in the width a posting stores it in.
///
/// Saturating rather than wrapping: a field with more than four billion tokens
/// would be reported as a shorter one, and a length that is wrong in that
/// direction makes every score computed from it wrong in the same direction.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A count as a float, without an `as` cast.
///
/// `f64` has no `From<u64>` because the conversion loses precision past 2^53,
/// and an `as` cast performs it silently. Splitting the value into halves that
/// convert exactly reaches the same number by an arithmetic that says so — and,
/// unlike saturating at `u32::MAX`, does not quietly report a large collection
/// as a smaller one, which would make every weight in it wrong.
fn as_float(count: u64) -> f64 {
    /// One more than the largest `u32`, as a float.
    const SHIFT: f64 = 4_294_967_296.0;
    let high = u32::try_from(count >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(count & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(high).mul_add(SHIFT, f64::from(low))
}

/// The value, when it is a usable positive number.
fn positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

#[cfg(test)]
mod tests;
