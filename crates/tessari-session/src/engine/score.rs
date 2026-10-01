//! BM25F: several fields scored as one document (ADR-0105 D3).
//!
//! Each field's occurrences are normalised by that field's own length against
//! that field's own average and weighed by the field's weight **before** the
//! saturation, so the fields add up to one term frequency and a word cannot
//! saturate once per field. Summing per-field BM25 scores instead would reward
//! a word for being spread across fields rather than for being held.
//!
//! One IDF per word, measured against every member the read reaches. A word
//! that is a prefix, a misspelling or has synonyms is one blended term: its
//! document frequency is the largest among the terms that answer it and its
//! occurrences are their sum (ADR-0104), so a rare variant cannot outrank the
//! common word it stands beside.
//!
//! With one field of weight one this is BM25 exactly, which is what lets the
//! suite compare a one-field search with the field index's own score.

use tessari_constants::{BM25_B, BM25_K1};

use super::query::{Answering, Probe};

/// What the collection looks like to one read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Collection {
    /// Documents across every member the read reaches.
    pub(crate) documents: f64,
}

/// One field of one record, as the score sees it.
pub(crate) struct Scored<'a> {
    /// How the field answers words.
    pub(crate) answering: &'a Answering,
    /// Its analysed terms.
    pub(crate) terms: &'a [String],
    /// Its weight.
    pub(crate) weight: f64,
    /// Its average length across its member's documents; `None` when the
    /// member holds no tokens in it, in which case nothing here can be held.
    pub(crate) average: Option<f64>,
}

/// The record's score against `words`, each beside its blended document
/// frequency.
pub(crate) fn bm25f(collection: Collection, words: &[(&Probe, f64)], fields: &[Scored<'_>]) -> f64 {
    let mut score = 0.0;
    for (probe, holding) in words {
        let mut weighted = 0.0;
        for field in fields {
            let Some(average) = field.average else {
                continue;
            };
            let occurrences = count(
                field
                    .terms
                    .iter()
                    .filter(|term| field.answering.answers(probe, term))
                    .count(),
            );
            if occurrences == 0.0 {
                continue;
            }
            let length = count(field.terms.len());
            let normalised = BM25_B.mul_add(length / average, 1.0 - BM25_B);
            weighted += field.weight * occurrences / normalised;
        }
        if weighted > 0.0 {
            score += idf(collection.documents, *holding) * weighted * (BM25_K1 + 1.0)
                / (BM25_K1 + weighted);
        }
    }
    score
}

/// The weight of a word held by `holding` of `documents` documents.
///
/// The form `rank.rs` uses, with the `1 +` that keeps a word held by most of
/// the collection from weighing below nothing.
fn idf(documents: f64, holding: f64) -> f64 {
    (1.0 + (documents - holding + 0.5) / (holding + 0.5)).ln()
}

/// A count as a float, without an `as` cast: the counts here are tokens in one
/// record, far below where `f64` stops being exact.
pub(crate) fn count(value: usize) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

/// A stored total as a float.
pub(crate) fn total(value: u64) -> f64 {
    const SHIFT: f64 = 4_294_967_296.0;
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(high).mul_add(SHIFT, f64::from(low))
}
