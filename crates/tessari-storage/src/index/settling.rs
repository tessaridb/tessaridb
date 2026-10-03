//! Settling the term and count deltas a batch of index changes leaves.

use super::Pending;
use crate::error::Result;
use crate::store::Store;
use tessari_constants::SEARCH_PREFIX_MINIMUM;
use tessari_encoding::{
    IndexAddress, SearchStatistics, SearchStatisticsKey, SearchSuffixKey, SearchSurfaceKey,
    SearchTermKey, StoreKey, StoreValue, TermStatistics,
};
use tessari_kv::WriteBatch;

/// How one log record moves an index's collection statistics.
///
/// Signed, and accumulated rather than written per mutation: a batch touching
/// one index a thousand times moves two counters a thousand times and writes
/// them once.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Delta {
    pub(crate) documents: i64,
    pub(crate) tokens: i64,
}

impl Delta {
    /// Record a document leaving the index at this length.
    pub(crate) fn removed(&mut self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.documents = self.documents.saturating_sub(1);
        self.tokens = self
            .tokens
            .saturating_sub(i64::try_from(tokens).unwrap_or(i64::MAX));
    }

    /// Record a document entering the index at this length.
    pub(crate) fn added(&mut self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.documents = self.documents.saturating_add(1);
        self.tokens = self
            .tokens
            .saturating_add(i64::try_from(tokens).unwrap_or(i64::MAX));
    }

    /// Whether anything moved.
    pub(crate) const fn is_zero(self) -> bool {
        self.documents == 0 && self.tokens == 0
    }
}

/// Fold the accumulated deltas into the stored statistics, one write per index.
///
/// The current values are read from **committed** state, which is the state this
/// log record is about to be applied on top of — the same state the previous
/// record values above were read at, so the two cannot describe different
/// moments. A store with one writer (ADR-0007) makes that read-modify-write safe
/// without a counter primitive.
pub(crate) fn settle(
    store: &Store,
    mut batch: WriteBatch,
    pending: &Pending,
) -> Result<WriteBatch> {
    let (moved, built) = (&pending.moved, &pending.built);
    let keyspace = SearchStatisticsKey::keyspace();
    for (address, delta) in moved {
        // A member's field totals can move while its two counts net to zero —
        // text moved from one field to another — so both are asked.
        let lengths = pending.lengths.get(address);
        if delta.is_zero() && lengths.is_none_or(|moved| moved.iter().all(|held| *held == 0)) {
            continue;
        }
        let key = SearchStatisticsKey::new(*address).encode();
        // An index this record **built** was counted whole, so its figure is a
        // total and starts from nothing. One that was merely updated carries a
        // movement, and starts from what is stored. Reading the stored figure
        // for a build would count every document twice — silently, since a
        // collection statistic has no reader who would notice it drifting.
        let (held, fields) = if built.contains(address) {
            (SearchStatistics::default(), Vec::new())
        } else {
            match store.backend().get(keyspace, &key)? {
                Some(bytes) => SearchStatistics::fielded(bytes.as_slice())?,
                None => (SearchStatistics::default(), Vec::new()),
            }
        };
        let updated = SearchStatistics::new(
            shift(held.documents, delta.documents),
            shift(held.terms, delta.tokens),
        );
        let value = match lengths {
            Some(moved) => {
                let totals: Vec<u64> = moved
                    .iter()
                    .enumerate()
                    .map(|(at, delta)| shift(fields.get(at).copied().unwrap_or(0), *delta))
                    .collect();
                updated.encode_fielded(&totals)
            }
            // A member this batch did not move a field of keeps its totals.
            None if !fields.is_empty() => updated.encode_fielded(&fields),
            None => updated.encode(),
        };
        batch = batch.put(keyspace, key, value);
    }
    settle_terms(store, batch, pending)
}

/// Fold the accumulated per-term deltas into the dictionary, one write per term
/// that moved.
///
/// # A term that reaches zero is deleted, not written as zero
///
/// The dictionary's whole purpose is that walking it enumerates the words the
/// index actually holds. An entry left behind at zero is a word a prefix walk
/// would return and whose posting list is empty — a suggestion nothing can
/// answer, offered by the structure built to stop exactly that. It also grows
/// without bound: every word ever written to the table stays in the dictionary
/// for the life of the store.
///
/// The read of the stored figure is skipped for an index this record **built**,
/// on the same reasoning [`settle`] gives for the collection statistics: a build
/// counted every row the index has, so its figure is a total and starting from
/// the stored one would count each document twice.
pub(crate) fn settle_terms(
    store: &Store,
    mut batch: WriteBatch,
    pending: &Pending,
) -> Result<WriteBatch> {
    let keyspace = SearchTermKey::keyspace();
    for (address, moved) in &pending.terms {
        let rebuilt = pending.built.contains(address);
        for (term, moved) in moved {
            // A rewrite that keeps a word nets a delta of zero and is still not
            // nothing: the word may now occur more often, or in a shorter
            // record, and the bound has to rise to cover it. Skipping on the
            // delta alone — which is what a count-only dictionary could do —
            // would leave a bound below a posting that exists, which is the one
            // direction ADR-0050 forbids.
            if moved.delta == 0 && moved.length.is_none() {
                continue;
            }
            let key = SearchTermKey::new(*address, term.clone()).encode();
            let held = if rebuilt {
                TermStatistics::default()
            } else {
                match store.backend().get(keyspace, &key)? {
                    Some(bytes) => TermStatistics::decode(bytes.as_slice())?,
                    None => TermStatistics::default(),
                }
            };
            let documents = shift(held.documents, moved.delta);
            // The term entering or leaving the dictionary is what moves its
            // suffixes, in this same batch, so an infix walk never reaches a
            // term the dictionary does not hold (ADR-0105 D9).
            if let Some(text) = term.as_text()
                && (documents == 0) != (held.documents == 0)
            {
                batch = suffixes(batch, address, &text, documents > 0);
            }
            batch = if documents == 0 {
                // The extremes leave with the entry, which is the one place they
                // are allowed to move inward. A term no record holds has no
                // postings to bound, so the next arrival starts from what it
                // actually writes rather than inheriting a ceiling from a record
                // that is gone.
                batch.delete(keyspace, key)
            } else {
                let frequency = held.max_frequency.max(moved.frequency);
                // `min` is not enough on its own, because zero is this field's
                // "never recorded" and would win every comparison — the sound
                // direction for a maximum and exactly backwards for a minimum.
                let length = match (held.min_length, moved.length) {
                    (0, arrived) => arrived.unwrap_or(0),
                    (held, Some(arrived)) => held.min(arrived),
                    (held, None) => held,
                };
                batch.put(
                    keyspace,
                    key,
                    TermStatistics::bounded(documents, frequency, length).encode(),
                )
            };
        }
    }
    settle_surfaces(store, batch, pending)
}

/// Fold the per-pair surface deltas into the surface dictionary (Q-867): one
/// read and one write per pair that moved, deleted at zero so a fuzzy walk
/// never reaches a spelling no record holds. A built index's figure is a total,
/// for the reason [`settle`] gives.
fn settle_surfaces(store: &Store, mut batch: WriteBatch, pending: &Pending) -> Result<WriteBatch> {
    let keyspace = SearchSurfaceKey::keyspace();
    for (address, moved) in &pending.surfaces {
        let rebuilt = pending.built.contains(address);
        for ((surface, term), delta) in moved {
            if *delta == 0 {
                continue;
            }
            let key = SearchSurfaceKey::new(*address, surface.clone(), term.clone()).encode();
            let held = if rebuilt {
                0
            } else {
                match store.backend().get(keyspace, &key)? {
                    Some(bytes) => SearchSurfaceKey::counted(bytes.as_slice())?,
                    None => 0,
                }
            };
            batch = match shift(held, *delta) {
                0 => batch.delete(keyspace, key),
                count => batch.put(keyspace, key, SearchSurfaceKey::count(count)),
            };
        }
    }
    Ok(batch)
}

/// Write or delete every suffix of `term` at least the prefix floor long.
fn suffixes(
    mut batch: WriteBatch,
    address: &IndexAddress,
    term: &str,
    arriving: bool,
) -> WriteBatch {
    let keyspace = SearchSuffixKey::keyspace();
    let length = term.chars().count();
    for (skipped, (at, _)) in term.char_indices().enumerate() {
        if length.saturating_sub(skipped) < SEARCH_PREFIX_MINIMUM {
            break;
        }
        let Some(suffix) = term.get(at..) else {
            continue;
        };
        let key = SearchSuffixKey::new(*address, suffix.to_owned(), term.to_owned()).encode();
        batch = if arriving {
            batch.put(keyspace, key, SearchSuffixKey::empty())
        } else {
            batch.delete(keyspace, key)
        };
    }
    batch
}

/// A count moved by a signed amount, without wrapping below zero.
pub(crate) fn shift(count: u64, delta: i64) -> u64 {
    if delta.is_negative() {
        count.saturating_sub(delta.unsigned_abs())
    } else {
        count.saturating_add(delta.unsigned_abs())
    }
}
