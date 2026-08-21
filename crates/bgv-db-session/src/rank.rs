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
//! (`bgv_db_encoding::SearchStatistics`).
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

use bgv_db_constants::{BM25_B, BM25_K1};
use bgv_db_types::{Analyzer, Number, Value};

/// What one searched field's collection looks like, resolved once per read.
///
/// The `frequencies` map holds only the terms the statement actually asks
/// about — resolving them all would be reading the index to answer a question
/// nobody put.
#[derive(Debug, Clone, Default)]
pub(crate) struct Corpus {
    /// How many records hold at least one term.
    pub(crate) documents: u64,
    /// The length of a typical document, in tokens.
    pub(crate) average_length: f64,
    /// How many documents hold each term the statement names.
    pub(crate) frequencies: BTreeMap<String, u64>,
}

/// The BM25 score of one record's text against one query.
///
/// Both texts are analysed with the field's own analyzer, so the words being
/// counted are the same words the index posted — the rule SGC.T1 exists for,
/// applied one layer up.
pub(crate) fn score(corpus: &Corpus, analyzer: &Analyzer, held: &Value, wanted: &Value) -> Value {
    let (Value::String(text), Value::String(query)) = (held, wanted) else {
        // Not text: it holds none of the words, which scores zero. The same
        // answer a document of the wrong shape gets from `MATCHES`, in the
        // ranking's own terms.
        return zero();
    };
    let asked = analyzer.terms(query);
    if asked.is_empty() || corpus.documents == 0 {
        return zero();
    }
    let Some(average) = positive(corpus.average_length) else {
        return zero();
    };

    let terms = analyzer.terms(text);
    let length = size(terms.len());
    let total = as_float(corpus.documents);

    let mut sum = 0.0_f64;
    for term in asked {
        let occurrences = size(terms.iter().filter(|held| **held == term).count());
        if occurrences == 0.0 {
            // A term the record does not hold contributes nothing, however rare
            // it is — so a query word that is in no document at all cannot lift
            // every score by the same amount and change nothing but the numbers.
            continue;
        }
        let frequency = as_float(*corpus.frequencies.get(&term).unwrap_or(&0));
        sum +=
            inverse_document_frequency(total, frequency) * saturation(occurrences, length, average);
    }
    Value::Number(Number::float(sum))
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

/// A score of nothing.
///
/// `0`, and deliberately not `NONE`: a document holding none of the query's
/// words scores zero, which is a computed answer rather than an absence standing
/// in for one. It also sorts where it belongs under the `DESC` a ranked read is
/// written with.
fn zero() -> Value {
    Value::Number(Number::float(0.0))
}

/// A length as a float.
fn size(value: usize) -> f64 {
    as_float(u64::try_from(value).unwrap_or(u64::MAX))
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
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_types::{Analyzer, Number, Value};

    use super::{Corpus, score};

    fn analyzer() -> Analyzer {
        Analyzer::default()
    }

    fn corpus(documents: u64, average_length: f64, frequencies: &[(&str, u64)]) -> Corpus {
        Corpus {
            documents,
            average_length,
            frequencies: frequencies
                .iter()
                .map(|(term, held)| ((*term).to_owned(), *held))
                .collect(),
        }
    }

    fn number(value: &Value) -> f64 {
        match value {
            Value::Number(Number::Float(held)) => *held,
            other => panic!("not a score: {other:?}"),
        }
    }

    fn scored(corpus: &Corpus, text: &str, query: &str) -> f64 {
        number(&score(
            corpus,
            &analyzer(),
            &Value::from(text),
            &Value::from(query),
        ))
    }

    #[test]
    fn holding_more_of_the_query_outranks_holding_less() {
        let corpus = corpus(100, 10.0, &[("lock", 20), ("contention", 5)]);
        let both = scored(&corpus, "lock contention here", "lock contention");
        let one = scored(&corpus, "lock here", "lock contention");
        assert!(both > one, "{both} vs {one}");
        assert!(one > 0.0);
    }

    #[test]
    fn a_rare_term_is_worth_more_than_a_common_one() {
        // The whole reason a score is not a count of matches. Two documents, one
        // word each, differing only in how many other documents hold that word.
        let corpus = corpus(1_000, 10.0, &[("rare", 2), ("common", 900)]);
        let scarce = scored(&corpus, "rare", "rare");
        let usual = scored(&corpus, "common", "common");
        assert!(scarce > usual, "{scarce} vs {usual}");
    }

    #[test]
    fn a_term_in_most_of_the_collection_still_counts_for_something() {
        // Textbook BM25 goes negative here, which would rank a document holding
        // the word *below* one that does not hold it at all.
        let corpus = corpus(100, 10.0, &[("the", 99)]);
        let held = scored(&corpus, "the", "the");
        assert!(held > 0.0, "{held}");
    }

    #[test]
    fn a_short_document_outranks_a_long_one_holding_the_term_as_often() {
        let corpus = corpus(100, 10.0, &[("lock", 10)]);
        let short = scored(&corpus, "lock", "lock");
        let long = scored(
            &corpus,
            "lock and a great deal of other prose about entirely unrelated matters",
            "lock",
        );
        assert!(short > long, "{short} vs {long}");
    }

    #[test]
    fn repetition_helps_and_then_stops_helping() {
        // `k1` is what makes the tenth occurrence worth less than the second;
        // without saturation a document could be lifted by repeating one word.
        let corpus = corpus(100, 10.0, &[("lock", 10)]);
        let once = scored(&corpus, "lock", "lock");
        let twice = scored(&corpus, "lock lock", "lock");
        let many = scored(&corpus, "lock lock lock lock lock lock lock lock", "lock");
        assert!(twice > once);
        assert!(many > twice);
        assert!(many - twice < twice - once, "saturation is not saturating");
    }

    #[test]
    fn a_record_holding_none_of_the_query_scores_zero() {
        let corpus = corpus(100, 10.0, &[("lock", 10)]);
        assert_eq!(scored(&corpus, "something else entirely", "lock"), 0.0);
        assert_eq!(scored(&corpus, "", "lock"), 0.0);
    }

    #[test]
    fn an_empty_collection_scores_every_record_the_same() {
        // Which is the true answer rather than a safe one: with nothing to
        // compare against, no record answers the query better than another.
        let corpus = corpus(0, 0.0, &[]);
        assert_eq!(scored(&corpus, "lock contention", "lock"), 0.0);
    }

    #[test]
    fn a_value_that_is_not_text_scores_zero_rather_than_failing() {
        let corpus = corpus(100, 10.0, &[("lock", 10)]);
        let held = score(
            &corpus,
            &analyzer(),
            &Value::Number(Number::from(7_i64)),
            &Value::from("lock"),
        );
        assert_eq!(number(&held), 0.0);
        let absent = score(&corpus, &analyzer(), &Value::None, &Value::from("lock"));
        assert_eq!(number(&absent), 0.0);
    }

    #[test]
    fn a_query_term_nobody_holds_lifts_nothing() {
        let corpus = corpus(100, 10.0, &[("lock", 10)]);
        let with = scored(&corpus, "lock", "lock unheardof");
        let without = scored(&corpus, "lock", "lock");
        assert!((with - without).abs() < f64::EPSILON, "{with} vs {without}");
    }

    #[test]
    fn the_frequencies_map_missing_a_term_treats_it_as_unseen() {
        // A resolution that failed to count a term must not silently score it as
        // if every document held it; an unlisted term is one no document holds.
        let corpus = corpus(100, 10.0, &[]);
        assert!(scored(&corpus, "lock", "lock") > 0.0);
    }
}
