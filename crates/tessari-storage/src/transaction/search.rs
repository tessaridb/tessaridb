//! Reads over terms, and the statistics a score needs.
//!
//! A term reaches records through its posting list; a prefix reaches them
//! through a bounded walk of the term dictionary. Both confirm against the
//! record, because an index entry is derived and the record is the fact.

use std::collections::{BTreeMap, BTreeSet};

use tessari_constants::{RANGE_SCAN_BATCH_ENTRIES, SEARCH_FUZZY_EXAMINATION_CAP};
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, Posting, PostingKey, SearchStatistics,
    SearchStatisticsKey, SearchTermKey, SecondaryIndexKey, StoreKey, StoreValue, TermStatistics,
    UniqueIndexKey, decode_payload,
};
use tessari_kv::{Key, KeyRange, Keyspace, ScanDirection, ScanRequest, Value as KvValue};
use tessari_types::{RecordId, Value, within_edits};

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
        Ok(holding.into_iter().collect())
    }

    /// The records any one of these terms is posted against.
    fn union(&self, address: &IndexAddress, terms: &[String]) -> Result<BTreeSet<RecordId>> {
        let mut found = BTreeSet::new();
        for term in terms {
            found.extend(self.postings(address, term)?);
        }
        Ok(found)
    }

    /// What a search index knows about its collection as a whole.
    ///
    /// An index that has never been written to has no statistics key, and the
    /// answer is the empty collection rather than an error: nothing is wrong
    /// with an index over no documents, and a caller ranking against one gets
    /// the same score for every record because that is the true answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn search_statistics(&self, index: &IndexDefinition) -> Result<SearchStatistics> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let key = SearchStatisticsKey::new(address).encode();
        match self
            .store
            .backend()
            .get(SearchStatisticsKey::keyspace(), &key)?
        {
            Some(bytes) => Ok(SearchStatistics::decode(bytes.as_slice())?),
            None => Ok(SearchStatistics::default()),
        }
    }

    /// How many documents this index posts the term against.
    ///
    /// The number a ranking weighs a term by. It is read from the term's
    /// dictionary entry — a **point read** — and only counted from the postings
    /// when there is no entry to read.
    ///
    /// # Why there are two paths and why the second one is not a fallback in the
    /// usual sense
    ///
    /// This was a count of the term's whole posting range: not materialised, but
    /// still a walk proportional to how many records hold the word, performed
    /// once per query term per query. On a common word in a large table that is
    /// the dominant cost of a ranked read, and it is spent to arrive at one
    /// integer the writer already knew.
    ///
    /// The dictionary holds that integer. An index written before the dictionary
    /// existed has none, and its terms have no entries — so the count is what
    /// answers there, and such an index keeps ranking correctly at the old cost
    /// rather than reporting every term as unheld. The two paths agree by
    /// construction: the entry is maintained in the same batch as the postings it
    /// counts, so a discrepancy is not a state this store can reach.
    ///
    /// A term nobody holds has no entry either, and the count it falls through to
    /// is a walk of an empty range — the cheapest read in the store.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored entry cannot be
    /// decoded.
    pub fn document_frequency(&self, index: &IndexDefinition, term: &str) -> Result<u64> {
        Ok(self.term_statistics(index, term)?.documents)
    }

    /// The term's whole dictionary entry — its frequency and, when the entry
    /// carries them, the extremes an upper bound is scored from.
    ///
    /// The same point read [`Transaction::document_frequency`] makes, reported
    /// without discarding the rest of what it read. A caller that prunes needs
    /// both, and reading the key twice to get them would spend the saving the
    /// dictionary exists for.
    ///
    /// The fall-through is the same one and means the same thing: no entry is an
    /// index written before the dictionary existed, and the count answers there.
    /// What it cannot answer is the extremes, so the statistics come back
    /// **unbounded** — which obliges a caller to score the term's postings
    /// rather than prune them (see [`TermStatistics::bound`]).
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored entry cannot be
    /// decoded.
    pub fn term_statistics(&self, index: &IndexDefinition, term: &str) -> Result<TermStatistics> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let encoded = IndexValues::of(&[Value::from(term)]);
        let key = SearchTermKey::new(address, encoded.clone()).encode();
        if let Some(bytes) = self.store.backend().get(SearchTermKey::keyspace(), &key)? {
            return Ok(TermStatistics::decode(bytes.as_slice())?);
        }
        let prefix = PostingKey::term_prefix(&address, &encoded);
        let counted = self
            .store
            .backend()
            .count(PostingKey::keyspace(), &KeyRange::prefix(&prefix))?;
        Ok(TermStatistics::new(counted))
    }

    /// The records one term is posted against.
    ///
    /// The candidate set a ranked read enumerates, one term at a time rather
    /// than as a union, because which terms are worth enumerating is decided
    /// between them — a term whose whole contribution cannot reach the answer's
    /// running threshold is one whose postings are never read.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_with_term(
        &self,
        index: &IndexDefinition,
        term: &str,
    ) -> Result<BTreeSet<RecordId>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        self.postings(&address, term)
    }

    /// What this index says one term does in one record.
    ///
    /// A **point read** of the posting, and the two numbers a score needs about
    /// the record it is scoring: how often the record holds the term, and how
    /// long the record's analysed field is. The writer knew both, and wrote both
    /// beside the membership they qualify (see [`Posting`]).
    ///
    /// `None` is the index not posting this record against this term — which for
    /// a score is the term contributing nothing, the same answer re-reading the
    /// record's text would reach by finding no occurrence of it.
    ///
    /// [`Posting::Membership`] is a posting written before the payload existed.
    /// It says the term is in the record and no more, so a caller that needs the
    /// numbers has to reach them another way; this method reports the distinction
    /// rather than resolving it, because only the caller knows what it can fall
    /// back to.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn posting(
        &self,
        index: &IndexDefinition,
        term: &str,
        id: &RecordId,
    ) -> Result<Option<Posting>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let encoded = IndexValues::of(&[Value::from(term)]);
        let key = PostingKey::new(address, encoded, id.clone()).encode();
        match self.store.backend().get(PostingKey::keyspace(), &key)? {
            Some(bytes) => Ok(Some(Posting::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// The distinct terms this index holds that begin with `prefix`, at most
    /// `cap` of them.
    ///
    /// A **bounded ordered walk of the term dictionary**, and the bound is the
    /// point. The entries are ordered by term and the encoded prefix selects a
    /// contiguous run, so the backend is asked for `cap + 1` entries of that run
    /// and stops. The work is therefore a function of how many terms **match**,
    /// never of how many terms the index holds — which is the property the whole
    /// dictionary exists to buy, and the one a prefix or fuzzy query is a denial
    /// of service without.
    ///
    /// `cap + 1` rather than `cap`, so the answer can say whether it was cut.
    /// A caller that got exactly `cap` terms and no signal cannot tell a complete
    /// expansion from a truncated one, and the two mean different things: the
    /// first is an answer, the second is an answer plus a silent omission.
    ///
    /// The terms come back as text. That is safe here and nowhere else in this
    /// module: a dictionary key is a lone string, which the index encoding
    /// reverses exactly (see [`IndexValues::as_text`]). An entry that is not one
    /// is skipped rather than guessed at.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn terms_with_prefix(
        &self,
        index: &IndexDefinition,
        prefix: &str,
        cap: usize,
    ) -> Result<Expansion> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let bounds = SearchTermKey::term_prefix(&address, prefix);
        let request = ScanRequest {
            keyspace: SearchTermKey::keyspace(),
            range: KeyRange::prefix(&bounds),
            direction: ScanDirection::Forward,
            limit: Some(cap.saturating_add(1)),
        };
        let found = self.store.backend().scan(&request)?;
        let capped = found.len() > cap;
        let mut terms = Vec::with_capacity(found.len().min(cap));
        for (key, _) in found.iter().take(cap) {
            if let Some(text) = SearchTermKey::decode(key.as_slice())?.term.as_text() {
                terms.push(text);
            }
        }
        let examined = terms.len();
        Ok(Expansion {
            terms,
            capped,
            examined,
        })
    }

    /// The terms within `edits` of `word` that share its first `prefix`
    /// characters.
    ///
    /// # The prefix is the query's semantics, and this walk only exploits it
    ///
    /// The restriction to terms sharing a leading run is **not** an optimisation
    /// this function is free to choose. It is part of what `MATCHES FUZZY`
    /// means, the scan applies the identical rule, and the two paths are asserted
    /// to agree record for record. That ordering matters: a bound that lived only
    /// here would make the same statement return one set on a table with no index
    /// and a smaller set once somebody declared one, which is the failure
    /// ADR-0046 exists to prevent.
    ///
    /// What this function gets from the restriction is that the walk is a
    /// **range read** — the terms sharing a prefix are contiguous in the
    /// dictionary — rather than a pass over the whole vocabulary.
    ///
    /// # Two numbers, because they are two different claims
    ///
    /// [`Expansion::examined`] counts what the walk read; `terms.len()` counts
    /// what survived the edit budget. For a prefix walk they are equal. Here they
    /// are not, and the gap is the honest cost of intersecting an automaton with
    /// a dictionary by walking a range instead of by seeking: a rarer word inside
    /// a common three-letter beginning reads its neighbours to find out they are
    /// not it.
    ///
    /// Both ceilings mark the expansion `capped` rather than raising, and a
    /// capped expansion is simply not offered as a candidate — the scan answers
    /// instead, with the identical result. A cap that refused would make a query
    /// succeed without an index and fail once somebody added one.
    pub fn terms_within_distance(
        &self,
        index: &IndexDefinition,
        word: &str,
        edits: usize,
        prefix: usize,
        cap: usize,
    ) -> Result<Expansion> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let leading: String = word.chars().take(prefix).collect();
        let bounds = SearchTermKey::term_prefix(&address, &leading);
        let request = ScanRequest {
            keyspace: SearchTermKey::keyspace(),
            range: KeyRange::prefix(&bounds),
            direction: ScanDirection::Forward,
            limit: Some(SEARCH_FUZZY_EXAMINATION_CAP.saturating_add(1)),
        };
        let found = self.store.backend().scan(&request)?;
        let mut capped = found.len() > SEARCH_FUZZY_EXAMINATION_CAP;
        let mut examined = 0usize;
        let mut terms = Vec::new();
        for (key, _) in found.iter().take(SEARCH_FUZZY_EXAMINATION_CAP) {
            let Some(text) = SearchTermKey::decode(key.as_slice())?.term.as_text() else {
                continue;
            };
            examined = examined.saturating_add(1);
            if !within_edits(word, &text, edits) {
                continue;
            }
            if terms.len() >= cap {
                capped = true;
                break;
            }
            terms.push(text);
        }
        Ok(Expansion {
            terms,
            capped,
            examined,
        })
    }

    /// The records one term is posted against.
    fn postings(&self, address: &IndexAddress, term: &str) -> Result<BTreeSet<RecordId>> {
        let encoded = IndexValues::of(&[Value::from(term)]);
        let prefix = PostingKey::term_prefix(address, &encoded);
        let mut found = BTreeSet::new();
        self.walk_range(
            PostingKey::keyspace(),
            &KeyRange::prefix(&prefix),
            |key, _| {
                found.insert(PostingKey::decode(key.as_slice())?.id);
                Ok(())
            },
        )?;
        Ok(found)
    }

    /// The records an index says hold a string beginning with `prefix`, as of
    /// this transaction's snapshot.
    ///
    /// A range read rather than a point read, and exact rather than approximate:
    /// a string encodes as its tag, its escaped bytes and a terminator, and the
    /// escape is byte-local, so the entries beginning with the encoded prefix are
    /// exactly the entries whose value begins with `prefix`. Nothing needs
    /// filtering out afterwards.
    ///
    /// Only the **first** indexed field is bounded, so this serves an index on
    /// that field and the leading field of a composite one. Candidates are
    /// confirmed at the reader's own snapshot exactly as
    /// [`Transaction::records_by_index`] does, with the same soundness and the
    /// same incompleteness at an older snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn records_with_string_prefix(
        &self,
        index: &IndexDefinition,
        prefix: &str,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let mut bounds = address.prefix(kind);
        bounds.extend_from_slice(&IndexValues::string_prefix(prefix));
        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        self.walk_range(kind.keyspace(), &KeyRange::prefix(&bounds), |key, value| {
            // A unique entry carries the record it points at in its value; a
            // secondary one carries it in its key.
            let id = if index.unique {
                IndexTarget::decode(value.as_slice())?.id
            } else {
                SecondaryIndexKey::decode(key.as_slice())?.id
            };
            let record = RecordAddress::new(index.namespace, index.database, index.table, id);
            if let Some(payload) = self.confirm_prefix(index, &record, prefix)? {
                found.insert(record.id, payload);
            }
            Ok(())
        })?;

        // The same reason as the equality path: a record this transaction wrote
        // has no entry yet, and one it changed still has the entry for its
        // former value.
        for pending in self.writes.keys() {
            if pending.namespace != index.namespace
                || pending.database != index.database
                || pending.table != index.table
            {
                continue;
            }
            match self.confirm_prefix(index, pending, prefix)? {
                Some(payload) => {
                    found.insert(pending.id.clone(), payload);
                }
                None => {
                    found.remove(&pending.id);
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    /// The record's payload, if it exists at the snapshot and its first indexed
    /// value is still a string beginning with `prefix`.
    fn confirm_prefix(
        &self,
        index: &IndexDefinition,
        address: &RecordAddress,
        prefix: &str,
    ) -> Result<Option<Vec<u8>>> {
        let Some(payload) = self.get(address)? else {
            return Ok(None);
        };
        let value = decode_payload(&payload)?;
        let wanted = IndexValues::string_prefix(prefix);
        // **Any** of the record's entries, because a multi-valued route gives it
        // several: the question is whether this record belongs in the answer, and
        // one entry beginning with the prefix is what makes it belong.
        if crate::index::project(index, &value)
            .iter()
            .any(|values| values.as_slice().starts_with(&wanted))
        {
            return Ok(Some(payload));
        }
        Ok(None)
    }

    /// The record ids the index entries point at, unconfirmed.
    ///
    /// A **complete** lookup on a unique index is a point read, because that is
    /// what unique means. Everything else is a prefix scan — including a leading
    /// lookup on a unique composite index, where one value of the first field
    /// may have many entries and a point read would find none of them.
    pub(super) fn candidates(
        &self,
        index: &IndexDefinition,
        address: &IndexAddress,
        values: &[Value],
        wanted: &[u8],
        complete: bool,
    ) -> Result<Vec<RecordId>> {
        if index.unique {
            if complete {
                let key = UniqueIndexKey::new(*address, IndexValues::of(values)).encode();
                let found = self.store.backend().get(UniqueIndexKey::keyspace(), &key)?;
                return found
                    .map(|bytes| Ok(IndexTarget::decode(bytes.as_slice())?.id))
                    .transpose()
                    .map(Vec::from_iter);
            }
            let mut prefix = address.prefix(KeyKind::UniqueIndex);
            prefix.extend_from_slice(wanted);
            let mut found = Vec::new();
            self.walk_range(
                UniqueIndexKey::keyspace(),
                &KeyRange::prefix(&prefix),
                |_, value| {
                    found.push(IndexTarget::decode(value.as_slice())?.id);
                    Ok(())
                },
            )?;
            return Ok(found);
        }

        let mut prefix = address.prefix(KeyKind::SecondaryIndex);
        prefix.extend_from_slice(wanted);
        let mut found = Vec::new();
        self.walk_range(
            SecondaryIndexKey::keyspace(),
            &KeyRange::prefix(&prefix),
            |key, _| {
                found.push(SecondaryIndexKey::decode(key.as_slice())?.id);
                Ok(())
            },
        )?;
        Ok(found)
    }

    /// The record's payload, if it exists at the snapshot and one of its entries
    /// begins with `wanted`.
    pub(super) fn confirm(
        &self,
        index: &IndexDefinition,
        address: &RecordAddress,
        wanted: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let Some(payload) = self.get(address)? else {
            return Ok(None);
        };
        let value = decode_payload(&payload)?;
        // The confirmation is what makes an index unable to change an answer: an
        // entry is a claim about a record, and this asks the record. Two things
        // widen it beyond equality and neither loosens it. A multi-valued route
        // gives a record several entries, so the claim is about **any** of them.
        // And a leading lookup asks about the first *k* values, so the claim is
        // that an entry **begins** with them — which for a complete lookup is
        // equality, because an index has a fixed arity.
        if crate::index::project(index, &value)
            .iter()
            .any(|held| held.as_slice().starts_with(wanted))
        {
            return Ok(Some(payload));
        }
        Ok(None)
    }
}
