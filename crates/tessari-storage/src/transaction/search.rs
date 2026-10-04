//! Reads over terms, and the statistics a score needs.
//!
//! A term reaches records through its posting list; a prefix reaches them
//! through a bounded walk of the term dictionary. Both confirm against the
//! record, because an index entry is derived and the record is the fact.

mod candidates;
mod counts;
mod expansions;
mod fielded;
mod statistics;
pub use counts::SearchCounts;
pub use fielded::FieldedPostings;
use std::collections::BTreeSet;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, Located, Posting, PostingKey,
    SearchStatistics, SearchStatisticsKey, SearchTermKey, SecondaryIndexKey, StoreKey, StoreValue,
    TermStatistics, UniqueIndexKey, decode_payload,
};
use tessari_kv::{Key, KeyRange, Keyspace, ScanDirection, ScanRequest, Value as KvValue};
use tessari_types::{Analyzer, RecordId, Value};

use super::{RecordAddress, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

/// The terms a prefix reached, and whether reaching them ran out of room.
///
/// The flag is not a diagnostic. An expansion that was cut answers a **different
/// question** from one that was not — "the first `cap` words beginning with this"
/// rather than "the words beginning with this" — and a caller that cannot tell
/// them apart reports a subset as a set. Every surface built on this carries the
/// distinction outward rather than resolving it here, because only the caller
/// knows whether a truncated expansion is a refusal or an approximation it is
/// allowed to declare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// The matching terms, in dictionary order, at most `cap` of them.
    pub terms: Vec<String>,
    /// Whether more terms matched than the cap allowed.
    pub capped: bool,
    /// How many terms the dictionary walk **read** to find them.
    ///
    /// For a prefix walk this is the same number as `terms.len()`, because the
    /// range *is* the answer and nothing read is discarded. For a fuzzy walk it
    /// is not: the walk reads every term sharing the mandatory prefix and keeps
    /// only those inside the edit budget. The two numbers are reported
    /// separately because G022's S3 is a claim about the second one, and a
    /// measurement that could not tell them apart would confirm a bound the
    /// walk does not have.
    pub examined: usize,
}

impl Transaction<'_> {
    /// Hand every pair of a range to `take`, reading it in bounded batches.
    ///
    /// # Why the walk is batched and what that does not promise
    ///
    /// It bounds the **entries held at once**, which a single `scan` of the
    /// whole range does not: that one decodes the range into a `Vec` before the
    /// first pair reaches the caller, so the memory is a function of what is
    /// stored rather than of what is being built.
    ///
    /// It does not bound whatever `take` accumulates. A caller collecting one
    /// record id per entry still ends up holding one per entry — that is the
    /// answer's size and a different question. What this removes is the copy of
    /// the range that existed *beside* the answer, values included, for callers
    /// that then threw the values away.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure, or `take`'s.
    fn walk_range(
        &self,
        keyspace: Keyspace,
        range: &KeyRange,
        mut take: impl FnMut(&Key, &KvValue) -> Result<()>,
    ) -> Result<()> {
        let mut remaining = range.clone();
        loop {
            let request = ScanRequest {
                keyspace,
                range: remaining.clone(),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES),
            };
            let batch = self.store.backend().scan(&request)?;
            for (key, value) in &batch {
                take(key, value)?;
            }
            // A short batch is the end of the range: the backend was asked for
            // a full one and had fewer to give.
            let Some((last, _)) = batch
                .last()
                .filter(|_| batch.len() >= RANGE_SCAN_BATCH_ENTRIES)
            else {
                return Ok(());
            };
            remaining = remaining.resuming_after(last);
        }
    }

    /// The records an index says hold `values`, as of this transaction's
    /// snapshot.
    ///
    /// # Sound, and not complete, at an older snapshot
    ///
    /// Index entries hold the **current** state — they carry no version, and an
    /// update removes the entry for the value it replaced. This method therefore
    /// treats them as candidates and confirms each one by re-deriving the
    /// record's indexed values at the reader's own snapshot, so a stale entry
    /// can never produce a row that does not match.
    ///
    /// What it cannot do is find a record that held `values` at the snapshot and
    /// has since changed: its entry is gone, so there is no candidate to
    /// confirm. A reader at the latest committed state is exact; an older one
    /// gets no wrong rows and may get fewer.
    ///
    /// Uncommitted writes of this transaction participate, because entries are
    /// derived at commit and a writer would otherwise be unable to find what it
    /// just wrote.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    /// The records a search index says hold **every** one of these terms.
    ///
    /// A candidate set, like every index read: each record is re-checked at the
    /// reader's own snapshot by the condition that asked, so a stale posting can
    /// never produce a row that does not match. The intersection is taken here
    /// rather than by the caller because a posting list per term is what the
    /// index holds, and narrowing before decoding is the whole saving.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_by_terms(
        &self,
        index: &IndexDefinition,
        analyzer: Option<&Analyzer>,
        terms: &[String],
    ) -> Result<Vec<RecordId>> {
        let Some(first) = terms.first() else {
            return Ok(Vec::new());
        };
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let mut holding = self.postings(&address, first)?;
        for term in terms.iter().skip(1) {
            if holding.is_empty() {
                break;
            }
            let next = self.postings(&address, term)?;
            holding.retain(|id| next.contains(id));
        }
        self.settle_pending(index, analyzer, &mut holding, |held| {
            terms.iter().all(|term| held.contains(term))
        })?;
        Ok(holding.into_iter().collect())
    }

    /// The records a search index says hold, for **every** expansion, at least
    /// one of its terms.
    ///
    /// The shape of a prefix query: a conjunction across the words that were
    /// typed and a disjunction within each word, because "a word beginning with
    /// `vecto`" is satisfied by `vector` or `vectors` or `vectorised` and the
    /// second word must be satisfied too.
    ///
    /// The union is taken **before** the intersection, and per expansion rather
    /// than over everything at once. Flattening the two levels into one list
    /// would turn the conjunction into a single disjunction, and the read would
    /// answer with every record holding any of the words — more rows, all of
    /// them plausible, none of them raising anything.
    ///
    /// An empty expansion is a word nothing begins with, and it empties the
    /// whole answer: the conjunction cannot be satisfied. That is checked before
    /// any posting is read.
    ///
    /// Candidates are confirmed against the record by the condition that asked,
    /// exactly as [`Transaction::records_by_terms`] leaves them to be.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_by_expansions(
        &self,
        index: &IndexDefinition,
        analyzer: Option<&Analyzer>,
        expansions: &[Vec<String>],
    ) -> Result<Vec<RecordId>> {
        let Some(first) = expansions.first() else {
            return Ok(Vec::new());
        };
        if expansions.iter().any(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let mut holding = self.union(&address, first)?;
        for expansion in expansions.iter().skip(1) {
            if holding.is_empty() {
                break;
            }
            let next = self.union(&address, expansion)?;
            holding.retain(|id| next.contains(id));
        }
        // The expansions were resolved from the term dictionary, which a pending
        // write has not reached — so a record written in this transaction is
        // judged against the terms the walk found, not against the walk. A word
        // this record alone would have contributed is therefore not expanded to,
        // which is the same subset the cap already makes possible and is why
        // this read declares itself bounded rather than exact.
        self.settle_pending(index, analyzer, &mut holding, |held| {
            expansions
                .iter()
                .all(|expansion| expansion.iter().any(|term| held.contains(term)))
        })?;
        Ok(holding.into_iter().collect())
    }

    /// Bring this transaction's own writes into a set the postings produced.
    ///
    /// Index entries are derived at commit, so a record written here has no
    /// posting yet and one it changed still has the postings of the text it
    /// replaced. Both are settled the way the equality, string-prefix and region
    /// reads settle them: by asking the record rather than the index, at this
    /// reader's own snapshot.
    ///
    /// Without this a writer could not find what it just wrote — and, worse, the
    /// failure was silent and one-directional: a record updated *into* a match
    /// simply had no candidate, and no re-test above could add one back.
    ///
    /// `analyzer` is the field's, the same one that produced the query's terms,
    /// so the two sides ask one question. `None` means the field declares none,
    /// in which case it contributes no terms and no pending record can match —
    /// the answer a scan gives for the same record.
    fn settle_pending(
        &self,
        index: &IndexDefinition,
        analyzer: Option<&Analyzer>,
        holding: &mut BTreeSet<RecordId>,
        holds: impl Fn(&[String]) -> bool,
    ) -> Result<()> {
        for pending in self.writes.keys() {
            if pending.namespace != index.namespace
                || pending.database != index.database
                || pending.table != index.table
            {
                continue;
            }
            let held = self.terms_now(index, analyzer, pending)?;
            if held.as_deref().is_some_and(&holds) {
                holding.insert(pending.id.clone());
            } else {
                holding.remove(&pending.id);
            }
        }
        Ok(())
    }

    /// The terms a record contributes to this index **now** — derived from the
    /// record at the reader's snapshot rather than read from the index.
    ///
    /// `None` for a record that is not there, does not hold the indexed path, or
    /// holds something that is not text: the same three ways a record
    /// contributes nothing at write time, asked in the same order so the two
    /// cannot answer differently.
    fn terms_now(
        &self,
        index: &IndexDefinition,
        analyzer: Option<&Analyzer>,
        address: &RecordAddress,
    ) -> Result<Option<Vec<String>>> {
        let (Some(analyzer), Some(path)) = (analyzer, index.fields.first()) else {
            return Ok(None);
        };
        let Some(payload) = self.get(address)? else {
            return Ok(None);
        };
        let record = decode_payload(&payload)?;
        let Some(Value::String(text)) = path.resolve(&record) else {
            return Ok(None);
        };
        Ok(Some(analyzer.terms(text)))
    }

    /// The records any one of these terms is posted against.
    fn union(&self, address: &IndexAddress, terms: &[String]) -> Result<BTreeSet<RecordId>> {
        let mut found = BTreeSet::new();
        for term in terms {
            found.extend(self.postings(address, term)?);
        }
        Ok(found)
    }
}
