//! What the query probably meant, when it named a term nothing holds.
//!
//! # The trigger is a term, not an empty answer
//!
//! The obvious trigger is "the read returned nothing", and it is wrong in a way
//! that only shows up in use. A three-word query with one word misspelled
//! usually returns *some* records: the conjunction fails, but a disjunction does
//! not, and after `OR` arrived a query may legitimately be one. A trigger keyed
//! on an empty answer would therefore stay silent on the common case — a reader
//! who typed one word badly and got a thinner answer than they meant — and fire
//! only on the rare one where every word was wrong.
//!
//! So the question is asked per term: a term the dictionary does not hold
//! contributed nothing to the answer, whatever else did, and is worth saying so
//! about whether or not records came back.
//!
//! # Excluded terms are left alone
//!
//! `NOT babbage` where nothing holds `babbage` is not a mistake. The reader
//! asked for the records without that word and got exactly those; correcting the
//! spelling of an exclusion would change what they asked for in the one
//! direction that removes records they wanted.
//!
//! # Nearest is two passes, on purpose
//!
//! The walk is asked for one edit before it is asked for two, rather than once
//! for two and sorted afterwards, because sorting afterwards needs this module
//! to compute an edit distance of its own — a second implementation of the
//! question the dictionary walk already answers, free to disagree with it. Two
//! passes cost two scans of a bounded prefix range, and only for a term nothing
//! holds, which is the rare path by construction.
//!
//! Within a pass the candidates are ranked by how many records hold them, then
//! by the term itself. Frequency rather than dictionary order because a spelling
//! suggestion should offer the word people actually wrote; the term itself as
//! the tie-break because an answer that depends on which of two equally common
//! words the scan reached first is not an answer.

use std::collections::BTreeMap;

use tessari_constants::{SEARCH_FUZZY_EXPANSION_CAP, SEARCH_FUZZY_MAX_EDITS, SEARCH_FUZZY_PREFIX};
use tessari_storage::{IndexDefinition, Transaction};
use tessari_types::{Analyzer, Path};

use super::query::{Asked, asked};
use crate::error::Result;
use crate::outcome::{Nearest, Suggestion};

/// The suggestion this read's searched paths earn, if any dictionary was asked.
///
/// `None` means no dictionary was consulted — every searched path either
/// declares no analyzer or has no `SEARCH` index behind it — and that is a
/// different answer from [`Suggestion::NothingNearer`], which is a dictionary
/// saying it holds everything asked for.
pub(crate) fn suggested(
    transaction: &mut Transaction<'_>,
    indexes: &[IndexDefinition],
    analyzers: &BTreeMap<Path, Analyzer>,
    queries: &[(Path, String)],
) -> Result<Option<Suggestion>> {
    let mut consulted = false;
    let mut nearest = Vec::new();
    for (path, text) in queries {
        let Some(analyzer) = analyzers.get(path) else {
            continue;
        };
        // The first search index on the path. Any of them holds the same terms
        // for the same field, so which one is asked cannot change the answer —
        // only which of two identical dictionaries is read.
        let Some(index) = indexes
            .iter()
            .find(|index| index.search && index.fields.first() == Some(path))
        else {
            continue;
        };
        consulted = true;
        // The same function the predicate and candidate generation read the
        // query with, so a suggestion cannot come to be about a different set of
        // terms than the read was about.
        let terms = match asked(analyzer, text) {
            Asked::Phrase { terms, .. } => terms,
            Asked::Boolean { required, .. } => required.into_iter().flatten().collect(),
        };
        for term in terms {
            if transaction.document_frequency(index, &term)? > 0 {
                continue;
            }
            if nearest.iter().any(|held: &Nearest| held.typed == term) {
                continue;
            }
            if let Some(instead) = nearby(transaction, index, &term)? {
                nearest.push(Nearest {
                    typed: term,
                    instead,
                });
            }
        }
    }
    if !consulted {
        return Ok(None);
    }
    Ok(Some(if nearest.is_empty() {
        Suggestion::NothingNearer
    } else {
        Suggestion::DidYouMean(nearest)
    }))
}

/// The held term nearest an unheld one, at one edit before two.
fn nearby(
    transaction: &mut Transaction<'_>,
    index: &IndexDefinition,
    term: &str,
) -> Result<Option<String>> {
    for edits in 1..=SEARCH_FUZZY_MAX_EDITS {
        let found = transaction.terms_within_distance(
            index,
            term,
            edits,
            SEARCH_FUZZY_PREFIX,
            SEARCH_FUZZY_EXPANSION_CAP,
        )?;
        let mut ranked = Vec::with_capacity(found.terms.len());
        for candidate in found.terms {
            let held = transaction.document_frequency(index, &candidate)?;
            ranked.push((held, candidate));
        }
        // Most-held first, then the term itself, so two equally common words
        // resolve the same way on every run and on every replica.
        ranked.sort_by(|(left, one), (right, other)| right.cmp(left).then_with(|| one.cmp(other)));
        if let Some((_, candidate)) = ranked.into_iter().next() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
