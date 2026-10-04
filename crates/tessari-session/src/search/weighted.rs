//! A fuzzy word scored as one term over the terms it reaches, each weighed by
//! how far it is from what was typed (G058 C3, Q-909).
//!
//! A score over a field the statement asks `MATCHES FUZZY` of measures the words
//! as that read reached them. Scoring the typed spelling instead gave every
//! misspelling `0` — the word it was meant to be is in the record, the typo is
//! not — so a ranked fuzzy read came back in store order. The terms within the
//! edit budget share one document frequency, the largest among them, for the
//! reason a starred word's do (ADR-0104): weighed separately, the rarer the
//! spelling the more each occurrence of it would be worth. Each occurrence then
//! counts `1 / (1 + edits)`, so an exact term always outweighs a corrected one.

use std::collections::BTreeMap;

use tessari_constants::{SEARCH_FUZZY_EXPANSION_CAP, SEARCH_FUZZY_PREFIX};
use tessari_storage::{IndexDefinition, Transaction};

use super::matching::{budget, edits_to};
use crate::error::Result;
use crate::rank::Blend;

/// The blend of one fuzzy word, given its spellings (as
/// [`tessari_types::Analyzer::prefixes`] makes them: the typed and the stemmed).
pub(super) fn fuzzy_blended(
    transaction: &Transaction<'_>,
    index: &IndexDefinition,
    alternatives: &[String],
) -> Result<Blend> {
    // Each reached term with the weight of its nearest spelling.
    let mut reached: BTreeMap<String, f64> = BTreeMap::new();
    for spelling in alternatives {
        let allowed = budget(spelling);
        let near = transaction.terms_within_distance(
            index,
            spelling,
            allowed,
            SEARCH_FUZZY_PREFIX,
            SEARCH_FUZZY_EXPANSION_CAP,
        )?;
        // A term reached through its surface (Q-867) is a stem nobody typed;
        // its distance was measured against the surface, which the dictionary
        // does not return, so it weighs as far as the word may reach.
        let surfaced = transaction
            .terms_by_surface(
                index,
                spelling,
                allowed,
                SEARCH_FUZZY_PREFIX,
                SEARCH_FUZZY_EXPANSION_CAP,
            )?
            .map(|found| found.terms)
            .unwrap_or_default();
        for term in near.terms.into_iter().chain(surfaced) {
            let edits = edits_to(alternatives, &term).unwrap_or(allowed);
            let weight =
                1.0 / f64::from(u32::try_from(edits).unwrap_or(u32::MAX).saturating_add(1));
            let held = reached.entry(term).or_insert(0.0);
            *held = held.max(weight);
        }
    }
    let mut ranked = Vec::with_capacity(reached.len());
    for (term, weight) in reached {
        ranked.push((transaction.document_frequency(index, &term)?, term, weight));
    }
    ranked
        .sort_by(|(left, one, _), (right, other, _)| right.cmp(left).then_with(|| one.cmp(other)));
    ranked.truncate(SEARCH_FUZZY_EXPANSION_CAP);
    Ok(Blend {
        prefix: alternatives.first().cloned().unwrap_or_default(),
        documents: ranked.first().map_or(0, |(held, _, _)| *held),
        weights: ranked.iter().map(|(_, _, weight)| *weight).collect(),
        expansions: ranked.into_iter().map(|(_, term, _)| term).collect(),
    })
}
