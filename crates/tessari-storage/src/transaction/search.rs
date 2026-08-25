//! Reads over terms, and the statistics a score needs.
//!
//! A term reaches records through its posting list; a prefix reaches them
//! through a bounded walk of the term dictionary. Both confirm against the
//! record, because an index entry is derived and the record is the fact.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, PostingKey, SearchStatistics,
    SearchStatisticsKey, SecondaryIndexKey, StoreKey, StoreValue, UniqueIndexKey, decode_payload,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value};

use super::{RecordAddress, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

impl Transaction<'_> {
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
    /// The count a ranking needs, and it is a count of the postings rather than
    /// a number kept beside them — the postings *are* the answer, so a
    /// maintained copy would be a second statement of one fact.
    ///
    /// The keys are **counted, not decoded**: a term held by a million records
    /// would otherwise cost a million record-id allocations to answer a question
    /// about the number one.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails.
    pub fn document_frequency(&self, index: &IndexDefinition, term: &str) -> Result<u64> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let encoded = IndexValues::of(&[Value::from(term)]);
        let prefix = PostingKey::term_prefix(&address, &encoded);
        let request = ScanRequest {
            keyspace: PostingKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let found = self.store.backend().scan(&request)?.len();
        Ok(u64::try_from(found).unwrap_or(u64::MAX))
    }

    /// The records one term is posted against.
    fn postings(&self, address: &IndexAddress, term: &str) -> Result<BTreeSet<RecordId>> {
        let encoded = IndexValues::of(&[Value::from(term)]);
        let prefix = PostingKey::term_prefix(address, &encoded);
        let request = ScanRequest {
            keyspace: PostingKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut found = BTreeSet::new();
        for (key, _) in self.store.backend().scan(&request)? {
            found.insert(PostingKey::decode(key.as_slice())?.id);
        }
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
        let request = ScanRequest {
            keyspace: kind.keyspace(),
            range: KeyRange::prefix(&bounds),
            direction: ScanDirection::Forward,
            limit: None,
        };

        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        for (key, value) in self.store.backend().scan(&request)? {
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
        }

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
            let request = ScanRequest {
                keyspace: UniqueIndexKey::keyspace(),
                range: KeyRange::prefix(&prefix),
                direction: ScanDirection::Forward,
                limit: None,
            };
            return self
                .store
                .backend()
                .scan(&request)?
                .into_iter()
                .map(|(_, value)| Ok(IndexTarget::decode(value.as_slice())?.id))
                .collect();
        }

        let mut prefix = address.prefix(KeyKind::SecondaryIndex);
        prefix.extend_from_slice(wanted);
        let request = ScanRequest {
            keyspace: SecondaryIndexKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        self.store
            .backend()
            .scan(&request)?
            .into_iter()
            .map(|(key, _)| Ok(SecondaryIndexKey::decode(key.as_slice())?.id))
            .collect()
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
