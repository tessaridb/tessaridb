//! Expanding a prefix or a misspelling into the terms an index holds.

use super::{Expansion, RecordAddress, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;
use std::collections::{BTreeMap, BTreeSet};
use tessari_constants::SEARCH_FUZZY_EXAMINATION_CAP;
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, PostingKey, SearchSuffixKey, SearchSurfaceKey,
    SearchTermKey, SecondaryIndexKey, StoreKey, StoreValue, decode_payload,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value, within_edits};

impl Transaction<'_> {
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

    /// The terms whose **surface** forms are within `edits` of `word` and share
    /// its first `prefix` characters (Q-867) — the raw-companion half of a
    /// fuzzy expansion.
    ///
    /// The surface dictionary holds only the pairs a stemmer changed, so this
    /// walk adds to [`Self::terms_within_distance`] and never replaces it: a word
    /// the stemmer left alone is its own surface and is found there. The terms
    /// answered are postings to read, and a record posted under one may hold it
    /// through a different surface — a candidate the caller re-tests, as every
    /// fuzzy candidate is. The two ceilings mean what they mean in the term walk.
    ///
    /// `None` when the index was built before surfaces were kept: it holds none,
    /// so the caller scans rather than answering less than the scan would.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn terms_by_surface(
        &self,
        index: &IndexDefinition,
        word: &str,
        edits: usize,
        prefix: usize,
        cap: usize,
    ) -> Result<Option<Expansion>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        if self
            .store
            .backend()
            .get(
                SearchSurfaceKey::keyspace(),
                &SearchSurfaceKey::marker(address),
            )?
            .is_none()
        {
            return Ok(None);
        }
        let leading: String = word.chars().take(prefix).collect();
        let request = ScanRequest {
            keyspace: SearchSurfaceKey::keyspace(),
            range: KeyRange::prefix(&SearchSurfaceKey::surface_prefix(&address, &leading)),
            direction: ScanDirection::Forward,
            limit: Some(SEARCH_FUZZY_EXAMINATION_CAP.saturating_add(1)),
        };
        let found = self.store.backend().scan(&request)?;
        let mut capped = found.len() > SEARCH_FUZZY_EXAMINATION_CAP;
        let mut terms = BTreeSet::new();
        for (key, _) in found.iter().take(SEARCH_FUZZY_EXAMINATION_CAP) {
            let pair = SearchSurfaceKey::decode(key.as_slice())?;
            if !within_edits(word, &pair.surface, edits) {
                continue;
            }
            if terms.len() >= cap && !terms.contains(&pair.term) {
                capped = true;
                break;
            }
            terms.insert(pair.term);
        }
        let examined = found.len().min(SEARCH_FUZZY_EXAMINATION_CAP);
        Ok(Some(Expansion {
            terms: terms.into_iter().collect(),
            capped,
            examined,
        }))
    }

    /// The records one term is posted against.
    /// The dictionary terms containing `piece`, read from the suffix keyspace
    /// (ADR-0105 D9) — `None` when this index was built before suffixes
    /// existed, so the caller scans rather than trusting a partial walk.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn terms_with_infix(
        &self,
        index: &IndexDefinition,
        piece: &str,
        cap: usize,
    ) -> Result<Option<Expansion>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let marker = SearchSuffixKey::new(address, String::new(), String::new()).encode();
        if self
            .store
            .backend()
            .get(SearchSuffixKey::keyspace(), &marker)?
            .is_none()
        {
            return Ok(None);
        }
        let request = ScanRequest {
            keyspace: SearchSuffixKey::keyspace(),
            range: KeyRange::prefix(&SearchSuffixKey::piece_prefix(&address, piece)),
            direction: ScanDirection::Forward,
            limit: Some(SEARCH_FUZZY_EXAMINATION_CAP.saturating_add(1)),
        };
        let found = self.store.backend().scan(&request)?;
        let mut capped = found.len() > SEARCH_FUZZY_EXAMINATION_CAP;
        let mut terms = BTreeSet::new();
        for (key, _) in found.iter().take(SEARCH_FUZZY_EXAMINATION_CAP) {
            terms.insert(SearchSuffixKey::decode(key.as_slice())?.term);
            if terms.len() > cap {
                capped = true;
                break;
            }
        }
        let examined = terms.len();
        Ok(Some(Expansion {
            terms: terms.into_iter().collect(),
            capped,
            examined,
        }))
    }

    pub(crate) fn postings(
        &self,
        address: &IndexAddress,
        term: &str,
    ) -> Result<BTreeSet<RecordId>> {
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
    pub(crate) fn confirm_prefix(
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
}
