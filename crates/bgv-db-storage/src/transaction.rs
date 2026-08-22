//! Transactions at snapshot isolation.
//!
//! A snapshot is one sequence number. Every read in a transaction seeks to that
//! sequence and takes the newest version at or before it, so the transaction
//! sees one consistent point in the store's history however long it runs.
//!
//! # What this level guarantees, and what it does not
//!
//! Guaranteed: reads are consistent as of the snapshot; a transaction sees its
//! own writes; and on a write-write race the first committer wins while the
//! loser writes nothing at all.
//!
//! **Not** guaranteed, and this is the level's defining limitation:
//!
//! - **Write skew.** Two transactions may each read a set, each find an
//!   invariant satisfied, each write a *different* key, and both commit —
//!   leaving the invariant violated with no conflict raised anywhere. Conflict
//!   detection is over what a transaction *wrote*, not over what it *read*.
//!   A caller that needs such an invariant materialises it into a key that both
//!   transactions write, which turns the skew into an ordinary detected
//!   conflict.
//! - **Phantoms.** Detection is per record, so a predicate re-evaluated later
//!   may match records that did not exist at snapshot time.
//!
//! Both are demonstrated by the test suite rather than described only here.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use bgv_db_constants::MAX_COMMIT_ATTEMPTS;
use bgv_db_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, LogRecord, Mutation, PostingKey, RecordKey,
    RecordValue, SearchStatistics, SearchStatisticsKey, SecondaryIndexKey, StoreKey, StoreValue,
    UniqueIndexKey, decode_payload,
};
use bgv_db_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId, Value};

use crate::catalog::IndexDefinition;
use crate::error::{Error, Result};
use crate::store::Store;

/// The first byte string after every key beginning with these bytes.
///
/// Incrementing the last byte that can be incremented, and dropping the trailing
/// `0xFF`s — the standard way to turn "everything with this prefix" into an
/// exclusive upper bound. All-`0xFF` bytes have no successor, and the answer is
/// then an empty vector, which `KeyRange::between` reads as unbounded above.
fn after(mut bytes: Vec<u8>) -> Vec<u8> {
    while let Some(last) = bytes.pop() {
        if last != u8::MAX {
            bytes.push(last.saturating_add(1));
            return bytes;
        }
    }
    bytes
}

/// Where a record lives: its table, and its identity within it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordAddress {
    /// The namespace.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The table within the database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
}

impl RecordAddress {
    /// Address a record.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: RecordId,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            id,
        }
    }

    fn key_at(&self, version: Sequence) -> RecordKey {
        RecordKey::new(
            self.namespace,
            self.database,
            self.table,
            self.id.clone(),
            version,
        )
    }

    fn versions_prefix(&self) -> Vec<u8> {
        RecordKey::versions_prefix(self.namespace, self.database, self.table, &self.id)
    }
}

/// A unit of work at a fixed snapshot.
///
/// Dropping one releases its snapshot. That is deliberately not left to
/// [`Transaction::commit`] and [`Transaction::rollback`]: the retention floor is
/// bounded by the oldest live snapshot, so a transaction that is simply let go
/// of without either call would freeze reclamation for the life of the process —
/// silently, as space that never comes back.
#[derive(Debug)]
pub struct Transaction<'a> {
    store: &'a Store,
    snapshot: Sequence,
    writes: BTreeMap<RecordAddress, RecordValue>,
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(store: &'a Store, snapshot: Sequence) -> Self {
        // Registered here rather than by the caller, so that a snapshot cannot
        // be read from without the store knowing it is being read from.
        store.snapshot_registry().register(snapshot);
        Self {
            store,
            snapshot,
            writes: BTreeMap::new(),
        }
    }

    /// The sequence every read in this transaction observes.
    ///
    /// Exposed because a snapshot's lifetime is an operational limit: while one
    /// is held, no version newer than it can be reclaimed.
    #[must_use]
    pub const fn snapshot(&self) -> Sequence {
        self.snapshot
    }

    /// Read a record as of this transaction's snapshot.
    ///
    /// Returns `None` when the record does not exist at that point, including
    /// when it was deleted at or before it.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn get(&self, address: &RecordAddress) -> Result<Option<Vec<u8>>> {
        if let Some(pending) = self.writes.get(address) {
            return Ok(match pending {
                RecordValue::Present(payload) => Some(payload.clone()),
                RecordValue::Tombstone => None,
            });
        }
        let found = self.read_at(address, self.snapshot)?;
        Ok(match found {
            Some((_, RecordValue::Present(payload))) => Some(payload),
            Some((_, RecordValue::Tombstone)) | None => None,
        })
    }

    /// Every live record of one table, as of this transaction's snapshot.
    ///
    /// Records come back in key order, with this transaction's own uncommitted
    /// writes folded in, and deleted records left out — a tombstone is a version
    /// like any other on disk, and a caller asking what is in a table does not
    /// want to hear about the rows that are not.
    ///
    /// **This reads the whole table.** It exists because the catalog is a table
    /// and the catalog is small; a scan that pages, and that a query plan can
    /// stop early, is a different piece of work and is not this one. Calling it
    /// on a table of unbounded size is a mistake this signature cannot prevent
    /// and this sentence is the warning.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn scan_table(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };

        // Versions of one record are adjacent and sort newest-first, so the
        // first version at or before the snapshot is the visible one and every
        // later entry for that record is an older version to walk past.
        let mut live: BTreeMap<RecordId, RecordValue> = BTreeMap::new();
        let mut resolved: Option<RecordId> = None;
        for (key, value) in self.store.backend().scan(&request)? {
            let decoded = RecordKey::decode(key.as_slice())?;
            if decoded.version > self.snapshot || resolved.as_ref() == Some(&decoded.id) {
                continue;
            }
            resolved = Some(decoded.id.clone());
            live.insert(decoded.id, RecordValue::decode(value.as_slice())?);
        }

        for (address, value) in &self.writes {
            if address.namespace == namespace
                && address.database == database
                && address.table == table
            {
                live.insert(address.id.clone(), value.clone());
            }
        }

        Ok(live
            .into_iter()
            .filter_map(|(id, value)| match value {
                RecordValue::Present(payload) => Some((id, payload)),
                RecordValue::Tombstone => None,
            })
            .collect())
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

    /// The records an ordered index holds between two bounds.
    ///
    /// # Both ends are inclusive, and that is not a limitation
    ///
    /// The index encoding **normalises** — `1`, `1.0` and `dec 1.00` become the
    /// same bytes — so the bytes equal to a bound are indistinguishable from the
    /// bound itself, and an exclusive byte bound cannot be expressed. It does not
    /// need to be: an index read is a **candidate set**, and the condition that
    /// asked is re-tested against every record it produces. So the scan takes
    /// both ends inclusive, over-fetching by at most the entries exactly equal to
    /// a bound, and `> x` discards those the way it discards everything else.
    ///
    /// Fewer moving parts than an exclusive byte bound, and provably the same
    /// answer.
    ///
    /// An absent bound is unbounded on that side, so one comparison serves as
    /// well as two.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_in_range(
        &self,
        index: &IndexDefinition,
        lower: Option<&Value>,
        upper: Option<&Value>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let start = match lower {
            Some(held) => {
                let mut bytes = prefix.clone();
                bytes.extend_from_slice(&IndexValues::leading(core::slice::from_ref(held)));
                bytes
            }
            None => prefix.clone(),
        };
        // The end is exclusive in `KeyRange::between`, and the bound itself must
        // be included — so the stop point is one byte past every key that begins
        // with the bound's encoding.
        let end = match upper {
            Some(held) => {
                let mut bytes = prefix.clone();
                bytes.extend_from_slice(&IndexValues::leading(core::slice::from_ref(held)));
                after(bytes)
            }
            None => after(prefix.clone()),
        };

        let request = ScanRequest {
            keyspace: kind.keyspace(),
            range: KeyRange::between(Key::from(start), Key::from(end)),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        for (key, value) in self.store.backend().scan(&request)? {
            let id = if index.unique {
                IndexTarget::decode(value.as_slice())?.id
            } else {
                SecondaryIndexKey::decode(key.as_slice())?.id
            };
            let record = RecordAddress::new(index.namespace, index.database, index.table, id);
            if let Some(payload) = self.get(&record)? {
                found.insert(record.id, payload);
            }
        }
        // A record this transaction wrote but has not committed has no index
        // entry yet, so it is folded in the way every other index read folds it.
        for (address, held) in &self.writes {
            if address.namespace != index.namespace
                || address.database != index.database
                || address.table != index.table
            {
                continue;
            }
            match held {
                RecordValue::Present(payload) => {
                    found.insert(address.id.clone(), payload.clone());
                }
                RecordValue::Tombstone => {
                    found.remove(&address.id);
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    /// The records a vector index says are nearest, nearest first.
    ///
    /// **Approximate**, and the only method on this type that is. A navigable
    /// graph returns the neighbours a greedy walk found, and showing that it
    /// missed none would mean the scan the index exists to avoid — which is why
    /// the language makes a statement ask for this before it may be used.
    ///
    /// A candidate set like every index read: each record is resolved at the
    /// reader's own snapshot, so a node left behind by a deleted record can
    /// never produce a row.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a node cannot be decoded.
    pub fn records_by_vector(
        &self,
        index: &IndexDefinition,
        query: &[f64],
        wanted: usize,
    ) -> Result<Vec<RecordId>> {
        let Some(distance) = index.vector else {
            return Ok(Vec::new());
        };
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let graph = crate::graph::Graph::read(self.store, &address, distance)?;
        Ok(graph.nearest(query, wanted))
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
    pub fn records_by_index(
        &self,
        index: &IndexDefinition,
        values: &[Value],
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        // The **leading** bytes, not the complete encoding, and one rule for
        // both cases. A complete encoding ends with a marker a longer key does
        // not carry in that position, so it is not a byte-prefix of a composite
        // index's key — which is why a composite index used to be offered for
        // nothing at all while being maintained on every write.
        //
        // For a complete lookup the leading bytes are the complete ones minus
        // that marker, and since an index has a fixed arity, "the record's entry
        // begins with these bytes" is equality there and a leading match here.
        let wanted = IndexValues::leading(values);
        let complete = values.len() == index.fields.len();

        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        for id in self.candidates(index, &address, values, &wanted, complete)? {
            let record = RecordAddress::new(index.namespace, index.database, index.table, id);
            if let Some(payload) = self.confirm(index, &record, &wanted)? {
                found.insert(record.id, payload);
            }
        }

        // A record this transaction wrote has no entry yet, and one it changed
        // still has the entry for its former value. Both are settled by asking
        // the pending write itself.
        for pending in self.writes.keys() {
            if pending.namespace != index.namespace
                || pending.database != index.database
                || pending.table != index.table
            {
                continue;
            }
            match self.confirm(index, pending, &wanted)? {
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
    fn candidates(
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
    fn confirm(
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

    /// Buffer a write. Nothing reaches the store until commit.
    pub fn put(&mut self, address: RecordAddress, payload: Vec<u8>) {
        self.writes.insert(address, RecordValue::Present(payload));
    }

    /// Buffer a delete.
    ///
    /// A delete is a version carrying a tombstone, not an erased key: a reader
    /// at an older snapshot must still see the record.
    pub fn delete(&mut self, address: RecordAddress) {
        self.writes.insert(address, RecordValue::Tombstone);
    }

    /// Discard the transaction.
    ///
    /// Nothing was written, so nothing is undone. Dropping the transaction does
    /// the same thing; this exists to say so at the call site.
    pub fn rollback(self) {
        drop(self);
    }

    /// Commit every buffered write at one new sequence.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] when another transaction committed to a
    /// record this one wrote, [`Error::CommitContention`] when every attempt
    /// lost the race for the committed tail, or a substrate error.
    pub fn commit(self) -> Result<Sequence> {
        if self.writes.is_empty() {
            return Ok(self.snapshot);
        }
        let record = self.log_record();

        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            if attempt > MAX_COMMIT_ATTEMPTS {
                return Err(Error::CommitContention {
                    attempts: MAX_COMMIT_ATTEMPTS,
                });
            }

            let tail = self.store.committed_tail()?;
            self.check_for_conflicts()?;
            // Inside the loop with the conflict check, and for the same reason:
            // both are read against the committed state this attempt builds on,
            // and a schema that moved between attempts must be re-read rather
            // than assumed.
            crate::schema::validate(self.store, &record)?;

            // Deciding the sequence locally is the *only* thing a commit does
            // that a replica's apply does not. Everything after this line is the
            // shared path.
            let commit_at = Sequence::new(tail.get().saturating_add(1));
            // Index entries are derived here rather than carried in the record,
            // and they are derived inside the loop because they depend on the
            // committed state this attempt is building on (see `crate::index`).
            let batch = crate::index::maintain(
                self.store,
                &record,
                crate::log::apply_batch(commit_at, &record),
            )?;
            match self.store.backend().apply(batch) {
                Ok(()) => return Ok(commit_at),
                // The position moved between reading it and applying, so the
                // conflict check above was made against a stale state and the
                // whole attempt is repeated rather than patched up.
                Err(bgv_db_kv::Error::Conflict { .. }) => continue,
                Err(other) => return Err(other.into()),
            }
        }
    }

    /// Everything this transaction changed, as the log will carry it.
    ///
    /// Built once, before the retry loop: the mutations do not depend on which
    /// sequence the commit eventually wins, so rebuilding them per attempt would
    /// be work that also invites the two attempts to differ.
    fn log_record(&self) -> LogRecord {
        LogRecord::new(
            self.writes
                .iter()
                .map(|(address, value)| Mutation {
                    namespace: address.namespace,
                    database: address.database,
                    table: address.table,
                    id: address.id.clone(),
                    value: value.clone(),
                })
                .collect(),
        )
    }

    /// Refuse the commit if any written record has moved since the snapshot.
    ///
    /// This is the write-write detection, and it is only sound because the
    /// commit batch asserts the tail has not moved either — together they turn
    /// check-then-write into a compare-and-set over the whole commit.
    fn check_for_conflicts(&self) -> Result<()> {
        for address in self.writes.keys() {
            let Some((version, _)) = self.read_newest(address)? else {
                continue;
            };
            if version > self.snapshot {
                return Err(Error::Conflict {
                    id: address.id.clone(),
                    snapshot: self.snapshot,
                    committed: version,
                });
            }
        }
        Ok(())
    }

    /// The newest version of a record, whatever its sequence.
    fn read_newest(&self, address: &RecordAddress) -> Result<Option<(Sequence, RecordValue)>> {
        let prefix = address.versions_prefix();
        self.first_in_range(KeyRange::prefix(&prefix))
    }

    /// The newest version of a record at or before `snapshot`.
    fn read_at(
        &self,
        address: &RecordAddress,
        snapshot: Sequence,
    ) -> Result<Option<(Sequence, RecordValue)>> {
        let prefix = address.versions_prefix();
        let bounds = KeyRange::prefix(&prefix);
        let range = KeyRange::from_bounds(
            Bound::Included(address.key_at(snapshot).encode()),
            bounds.end().clone(),
        );
        self.first_in_range(range)
    }

    fn first_in_range(&self, range: KeyRange) -> Result<Option<(Sequence, RecordValue)>> {
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range,
            direction: ScanDirection::Forward,
            limit: Some(1),
        };
        let found = self.store.backend().scan(&request)?;
        let Some((key, value)) = found.first() else {
            return Ok(None);
        };
        let decoded_key = RecordKey::decode(key.as_slice())?;
        let decoded_value = RecordValue::decode(value.as_slice())?;
        Ok(Some((decoded_key.version, decoded_value)))
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        self.store.snapshot_registry().release(self.snapshot);
    }
}
