//! Which of a record's tokens the read's own query reached.
//!
//! # The marks come from the read, never from a second copy of the query
//!
//! The tempting surface is `search::highlight(field, 'query')`, mirroring
//! `search::score`. It is wrong, and the reason is the one this store keeps
//! meeting: a second copy of a question can disagree with the first. A
//! projection repeating the query text can differ from the `WHERE` in its
//! spelling, in its slop, and — worst, because it is invisible — in the operator
//! it implies, since `MATCHES` and `MATCHES FUZZY` reach different terms from
//! the same word. A read whose highlight disagrees with its own filter is worse
//! than one with no highlight at all: it says a record matched somewhere it did
//! not.
//!
//! So the marks are computed from what *this read* asked of *this field*,
//! recorded once during rewrite in [`Searched`](super::Searched) and replayed
//! here.
//!
//! # And the rule they are computed with is the predicate's own
//!
//! Everything below defers to [`super::matching`] for whether a stored term
//! answers a typed word. Written separately the two would agree until a cap, an
//! edit budget or a filter moved, and then a record would come back with nothing
//! marked in it, or with a token marked that no operator reached. That failure
//! is silent — the record still matched — which is why the rule lives in one
//! place and this module only decides *which tokens* it applies to.

use core::ops::Range;

use tessari_ql::BinaryOp;
use tessari_types::Analyzer;

use super::matching::{begins, near, run_of};
use super::query::{Asked, asked};

/// The byte ranges of the tokens that answered what this read asked of the
/// field: ordered by position, without duplicates, one per matched occurrence.
///
/// A field nobody asked about answers nothing — not because the marking failed,
/// but because there was no question to mark against.
pub(crate) fn marked(
    analyzer: &Analyzer,
    text: &str,
    wanted: &[(BinaryOp, String)],
) -> Vec<Range<usize>> {
    let tokens = analyzer.spans(text);
    let terms: Vec<String> = tokens.iter().map(|token| token.term.clone()).collect();
    let mut reached = vec![false; tokens.len()];
    for (op, query) in wanted {
        mark(analyzer, *op, query, &terms, &mut reached);
    }
    tokens
        .into_iter()
        .zip(reached)
        .filter(|(_, hit)| *hit)
        .map(|(token, _)| token.bytes)
        .collect()
}

/// Mark the tokens one operator reached, leaving the ones already marked alone.
///
/// Several predicates may name the same field — `body MATCHES 'ada' OR body
/// MATCHES FUZZY 'lovelac'` — and a token either was reached or was not, so the
/// marks accumulate rather than replacing one another.
fn mark(analyzer: &Analyzer, op: BinaryOp, query: &str, terms: &[String], reached: &mut [bool]) {
    match op {
        BinaryOp::Matches => match asked(analyzer, query) {
            // A phrase marks its **run**. A record holding `lovelace ada … ada
            // lovelace` answers `"ada lovelace"` once, and marking all four
            // tokens would claim it answered twice.
            Asked::Phrase { terms: run, slop } => {
                for at in run_of(terms, &run, slop).unwrap_or_default() {
                    if let Some(hit) = reached.get_mut(at) {
                        *hit = true;
                    }
                }
            }
            // Only the **required** terms. An excluded one is never marked
            // because a matching record holds none of them, and marking one
            // would be reporting the reason a record was rejected as the reason
            // it was returned.
            Asked::Boolean { required, .. } => {
                for (hit, term) in reached.iter_mut().zip(terms) {
                    *hit |= required.iter().any(|group| group.contains(term));
                }
            }
        },
        BinaryOp::MatchesPrefix => {
            let asked = analyzer.prefixes(query);
            for (hit, term) in reached.iter_mut().zip(terms) {
                *hit |= asked.iter().any(|word| begins(word, term));
            }
        }
        BinaryOp::MatchesFuzzy => {
            let asked = analyzer.prefixes(query);
            for (hit, term) in reached.iter_mut().zip(terms) {
                *hit |= asked.iter().any(|word| near(word, term));
            }
        }
        _ => {}
    }
}
