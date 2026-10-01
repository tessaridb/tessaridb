//! A phrase decided from the ordinals a `POSITIONS` index stored (ADR-0100 D4).
//!
//! The scan decides a phrase by analysing the record's text and walking its
//! token list for the run. A `POSITIONS` index stored each term's ordinals in
//! that same list, so the walk can run over a list rebuilt from the postings of
//! the phrase's own terms instead — every other slot empty, which no word
//! answers — and reach the same verdict without reading the text. That is what
//! lets the read claim to answer the phrase, so the condition is not re-tested.

use std::collections::BTreeMap;

use tessari_storage::{IndexDefinition, Transaction};
use tessari_types::{Analyzer, RecordId, Value};

use crate::error::Result;
use crate::search::{Word, holds_run};

/// Whether the record holds the phrase.
///
/// From the stored ordinals of the terms each word reaches; from the record's
/// text when a posting of this record carries none, which an index keeping
/// positions never writes but which this answer does not depend on.
pub(super) fn holds_phrase(
    transaction: &Transaction<'_>,
    index: &IndexDefinition,
    analyzer: Option<&Analyzer>,
    (groups, words, slop): (&[Vec<String>], &[Word], usize),
    id: &RecordId,
    payload: &[u8],
) -> Result<bool> {
    let mut placed: BTreeMap<usize, &String> = BTreeMap::new();
    for term in groups.iter().flatten() {
        let Some(located) = transaction.located(index, term, id)? else {
            continue;
        };
        if located.positions.is_empty() {
            return from_text(index, analyzer, (words, slop), payload);
        }
        for position in located.positions {
            placed.insert(usize::try_from(position).unwrap_or(usize::MAX), term);
        }
    }
    let Some(last) = placed.keys().next_back() else {
        return Ok(false);
    };
    let mut held = vec![String::new(); last.saturating_add(1)];
    for (position, term) in placed {
        if let Some(slot) = held.get_mut(position) {
            slot.clone_from(term);
        }
    }
    Ok(holds_run(&held, words, slop))
}

/// The scan's own verdict, over the record's analysed text.
fn from_text(
    index: &IndexDefinition,
    analyzer: Option<&Analyzer>,
    (words, slop): (&[Word], usize),
    payload: &[u8],
) -> Result<bool> {
    let (Some(analyzer), Some(path)) = (analyzer, index.fields.first()) else {
        return Ok(false);
    };
    let value = tessari_encoding::decode_payload(payload)?;
    let Some(Value::String(text)) = path.resolve(&value) else {
        return Ok(false);
    };
    Ok(holds_run(&analyzer.terms(text), words, slop))
}
