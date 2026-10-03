//! Ranking a `FROM SEARCH` member from its postings alone (Q-870).
//!
//! A member posting carries, per field, how often its term occurs there and how
//! long the field is. For a query whose every word is a term, a prefix or an
//! infix — the words the dictionary expands exactly — that is everything the
//! re-test and BM25F read from the record's text: which field holds which word,
//! and each field's length. So such a read decides and scores from the postings
//! and reads only the records it answers, where the text path read and analysed
//! every candidate.
//!
//! The text path stays for everything the postings cannot decide: a phrase
//! (positions), a fuzzy word (it is measured against surfaces), a `WHERE` (it
//! reads the record), a transaction's own writes (they have no postings yet), a
//! snapshot older than the index, and a member written before postings kept
//! fields. Both paths go through one BM25F (`score::combined`), and the suite
//! asserts they rank every record with the same bits.

use std::collections::BTreeSet;

use tessari_storage::FieldedPostings;
use tessari_types::RecordId;

use super::query::{Probe, Query, Shape};
use super::read::Member;
use super::score::{Collection, combined, count};

/// Whether the postings decide this query's shape: a boolean query whose words
/// the dictionary expands exactly.
pub(super) fn decidable(query: &Query) -> bool {
    matches!(query.shape, Shape::Boolean { .. })
        && query
            .probes()
            .iter()
            .all(|probe| !matches!(probe, Probe::Fuzzy(_)))
}

/// Every record of one member that answers the query, with its score, decided
/// from `postings` — the member's postings of every term the query's words
/// expand to.
///
/// `expansions` is, per probe of [`Query::probes`], the terms its walk reached.
pub(super) fn ranked(
    member: &Member,
    query: &Query,
    expansions: &[BTreeSet<String>],
    postings: &FieldedPostings,
    (collection, holding): (Collection, &[(&Probe, f64)]),
) -> Vec<(RecordId, f64)> {
    let probes = query.probes();
    let Shape::Boolean { required, excluded } = &query.shape else {
        return Vec::new();
    };
    let reached = |probe: &Probe| {
        probes
            .iter()
            .position(|held| *held == probe)
            .and_then(|at| expansions.get(at))
    };
    let mut answered = Vec::new();
    for (id, held) in postings {
        // How often `probe` is answered in field `at` of this record: the terms
        // the field lets answer it, summed over the field's frequencies.
        let occurrences = |probe: &Probe, at: usize| -> f64 {
            let Some(field) = member.fields.get(at) else {
                return 0.0;
            };
            let Some(terms) = reached(probe) else {
                return 0.0;
            };
            let answering = &field.answering;
            terms
                .iter()
                .filter(|term| match probe {
                    Probe::Term(asked) => {
                        *term == asked
                            || answering
                                .synonyms
                                .get(asked)
                                .is_some_and(|alternatives| alternatives.contains(term))
                    }
                    Probe::Prefix(_) | Probe::Infix(_) => answering.prefix,
                    Probe::Fuzzy(_) => false,
                })
                .filter_map(|term| held.get(term)?.get(at))
                .map(|(frequency, _)| f64::from(*frequency))
                .sum()
        };
        let anywhere =
            |probe: &Probe| (0..member.fields.len()).any(|at| occurrences(probe, at) > 0.0);
        let holds = !required.is_empty()
            && required.iter().all(|group| group.iter().any(anywhere))
            && !excluded.iter().any(anywhere);
        if !holds {
            continue;
        }
        let lengths = held.values().next().cloned().unwrap_or_default();
        let shapes: Vec<(f64, Option<f64>, f64)> = member
            .fields
            .iter()
            .enumerate()
            .map(|(at, field)| {
                let length = lengths.get(at).map_or(0, |(_, length)| *length);
                (
                    field.weight,
                    field.average,
                    count(usize::try_from(length).unwrap_or(usize::MAX)),
                )
            })
            .collect();
        let score = combined(collection, holding, &shapes, |word, at| {
            holding
                .get(word)
                .map_or(0.0, |(probe, _)| occurrences(probe, at))
        });
        answered.push((id.clone(), score));
    }
    answered
}
