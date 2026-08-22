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

use bgv_db_constants::{
    DESCENDING_SCAN_BATCH_ENTRIES, MAX_COMMIT_ATTEMPTS, RANGE_SCAN_BATCH_ENTRIES,
};
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

/// The smallest key strictly greater than this one.
///
/// Appending a zero byte, which is the immediate successor in byte order: a key
/// above `bytes` either extends it — and the shortest extension is this one — or
/// differs from it earlier, and is then above every extension of it. So a scan
/// resuming here sees every remaining key and re-reads none.
///
/// Deliberately **not** [`after`], which is the successor of the whole *prefix*
/// and skips every key carrying `bytes` as a byte prefix. Today no index key
/// carries another as a prefix, because every variable-width component of one is
/// terminated when it is encoded — so `after` would work here. That property
/// lives in the encoder, a crate away from this loop, and a walk that silently
/// returns fewer records if it ever changes is not worth the byte it saves.
fn resuming_after(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.push(0);
    bytes
}

/// A record as an index read hands it back: its identity, and its stored bytes.
///
/// Named because [`Transaction::records_in_descending_order`] answers with an
/// *optional* list of them, and a nested triple of generics is a signature
/// nobody reads twice.
pub type StoredRecord = (RecordId, Vec<u8>);

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

    /// Read several records as of this transaction's snapshot, in one ask.
    ///
    /// Answers exactly as [`Self::get`] called on each address would — pending
    /// writes folded in the same way, tombstones absent the same way — and
    /// returns one entry per address, in order.
    ///
    /// It exists because resolving the records an index names is the place
    /// where the cost of reading one record is multiplied by the size of an
    /// answer. Each record read is a bounded range rather than a point lookup,
    /// because records are versioned and the visible one is the newest at or
    /// below the snapshot; asking for those ranges together lets a backend set
    /// up once instead of once per record.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn get_each(&self, addresses: &[RecordAddress]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut answers: Vec<Option<Vec<u8>>> = vec![None; addresses.len()];
        let mut ranges = Vec::new();
        let mut asked = Vec::new();
        for (index, address) in addresses.iter().enumerate() {
            if let Some(pending) = self.writes.get(address) {
                if let RecordValue::Present(payload) = pending {
                    answers[index] = Some(payload.clone());
                }
                continue;
            }
            let bounds = KeyRange::prefix(&address.versions_prefix());
            ranges.push(KeyRange::from_bounds(
                Bound::Included(address.key_at(self.snapshot).encode()),
                bounds.end().clone(),
            ));
            asked.push(index);
        }

        let found = self
            .store
            .backend()
            .first_of_each(RecordKey::keyspace(), &ranges)?;
        for (index, pair) in asked.into_iter().zip(found) {
            let Some((_, value)) = pair else { continue };
            if let RecordValue::Present(payload) = RecordValue::decode(value.as_slice())? {
                answers[index] = Some(payload);
            }
        }
        Ok(answers)
    }

    /// Every live record of one table, as of this transaction's snapshot.
    ///
    /// Records come back in key order, with this transaction's own uncommitted
    /// writes folded in, and deleted records left out — a tombstone is a version
    /// like any other on disk, and a caller asking what is in a table does not
    /// want to hear about the rows that are not.
    ///
    /// **This reads the whole table.** It exists because the catalog is a table
    /// and the catalog is small. A read that knows how many records it needs
    /// asks [`Self::first_records_of`] instead; calling this one on a table of
    /// unbounded size is a mistake this signature cannot prevent and this
    /// sentence is the warning.
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
        self.table_records(namespace, database, table, None)
    }

    /// The first `wanted` live records of one table, in key order.
    ///
    /// # The contract, which is looser than it looks and deliberately so
    ///
    /// Returns **at least** `wanted` records, or every record there is when the
    /// table holds fewer. It may return more, and a caller that asked for a
    /// bound still applies it. An over-return costs a little memory; an
    /// under-return is a **quietly short answer** — the right records, fewer of
    /// them, with nothing raised — so the arithmetic below is deliberately loose
    /// in the safe direction.
    ///
    /// # Why the count is not simply handed to the backend
    ///
    /// Two reasons, and both are the kind that produce a plausible wrong answer
    /// rather than a failure.
    ///
    /// A scan's limit counts **entries**, and a record has as many entries as it
    /// has versions. Asking for `wanted` entries would return fewer than
    /// `wanted` records whenever anything had been updated. So the walk asks for
    /// what it still needs, batch by batch, and counts records rather than rows.
    ///
    /// And this transaction's own uncommitted writes are folded in afterwards,
    /// where a **tombstone** removes a record the walk already counted and
    /// leaves the answer one short. An insert cannot do the same damage — it
    /// only makes the set larger, and the caller's own bound truncates it — so
    /// over-fetching by the number of pending tombstones on this table is
    /// enough, and in the ordinary case, a read outside a write transaction, it
    /// is exactly `wanted`.
    ///
    /// That asymmetry was not obvious and is recorded because it was found the
    /// hard way: a test written to exercise the displacement used one insert and
    /// one delete, which cancelled, and passed with the over-fetch removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn first_records_of(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        wanted: usize,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.table_records(namespace, database, table, Some(wanted))
    }

    /// The live records of one table, all of them or the first `bound` of them.
    fn table_records(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        bound: Option<usize>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let of_this_table = |address: &RecordAddress| {
            address.namespace == namespace && address.database == database && address.table == table
        };
        let wanted = bound.map(|bound| {
            let displacing = self
                .writes
                .iter()
                .filter(|(address, value)| {
                    of_this_table(address) && matches!(value, RecordValue::Tombstone)
                })
                .count();
            bound.saturating_add(displacing)
        });

        // Versions of one record are adjacent and sort newest-first, so the
        // first version at or before the snapshot is the visible one and every
        // later entry for that record is an older version to walk past.
        //
        // `resolved` carries across batches for that reason: a record's versions
        // may straddle a boundary, and forgetting which record was just settled
        // would let an older version of it be read as a newer record.
        let mut live: BTreeMap<RecordId, RecordValue> = BTreeMap::new();
        let mut resolved: Option<RecordId> = None;
        // Counted separately from `live.len()`, which includes the tombstones
        // that are about to be filtered out. Stopping on a count that includes
        // them would answer short by however many deleted records the walk
        // happened to pass.
        let mut present = 0_usize;
        let mut from = prefix.clone();
        let end = after(prefix);
        loop {
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::between(Key::from(from), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                // The unbounded read stays one scan of everything, unchanged.
                // Only a read that knows what it needs asks for less, and it
                // asks for exactly what it still needs so a small bound costs a
                // small scan rather than a batch-sized one.
                limit: wanted.map(|wanted| {
                    wanted
                        .saturating_sub(present)
                        .clamp(1, RANGE_SCAN_BATCH_ENTRIES)
                }),
            };
            let batch = self.store.backend().scan(&request)?;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            let entries = batch.len();
            for (key, value) in batch {
                let decoded = RecordKey::decode(key.as_slice())?;
                if decoded.version > self.snapshot || resolved.as_ref() == Some(&decoded.id) {
                    continue;
                }
                resolved = Some(decoded.id.clone());
                let value = RecordValue::decode(value.as_slice())?;
                if matches!(value, RecordValue::Present(_)) {
                    present = present.saturating_add(1);
                }
                live.insert(decoded.id, value);
            }
            let Some(wanted) = wanted else {
                break;
            };
            // A batch shorter than asked for is the end of the table; otherwise
            // the walk continues until it has what it came for.
            let Some(last) = last.filter(|_| present < wanted && entries > 0) else {
                break;
            };
            from = resuming_after(last);
        }

        for (address, value) in &self.writes {
            if of_this_table(address) {
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
    /// # Two costs, bounded separately
    ///
    /// A range has no early stop — every entry between the bounds belongs to the
    /// answer — so batching buys no skipped work. It bounds two different things.
    ///
    /// **What is held at once.** The entries stop being proportional to the
    /// width of the range, which is a bound rather than a saving: measured over
    /// fifty thousand entries it is about four per cent of the read's peak, and
    /// at five million it is the difference between tens of kilobytes and
    /// hundreds of megabytes. The **records** are still all held — they are the
    /// answer, and the caller re-tests the condition that asked against every
    /// one of them, so bounding them would change the shape of an answer rather
    /// than this read. `docs/bgvql.md` §8 records that with the numbers.
    ///
    /// **What is asked of the backend.** Each entry names a record, and reading
    /// a record is itself a bounded range because records are versioned. Asking
    /// for those one at a time costs one backend round trip per record — a cost
    /// proportional to the answer, and on an engine one iterator per record,
    /// each pinning the store's view while it lives. The records a batch of
    /// entries names are therefore resolved together through
    /// [`Self::get_each`], so the whole read costs two round trips per entry
    /// batch rather than one per record.
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

        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        let mut from = start;
        loop {
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::between(Key::from(from), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES),
            };
            let batch = self.store.backend().scan(&request)?;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            // The entries name the records; the records are then read together.
            // Reading each one as it is named would be the same answer at one
            // backend round trip per record, which is a cost that grows with the
            // answer and is what `get_each` exists to avoid.
            let mut addresses = Vec::with_capacity(batch.len());
            for (key, value) in &batch {
                let id = if index.unique {
                    IndexTarget::decode(value.as_slice())?.id
                } else {
                    SecondaryIndexKey::decode(key.as_slice())?.id
                };
                addresses.push(RecordAddress::new(
                    index.namespace,
                    index.database,
                    index.table,
                    id,
                ));
            }
            for (address, payload) in addresses.iter().zip(self.get_each(&addresses)?) {
                if let Some(payload) = payload {
                    found.insert(address.id.clone(), payload);
                }
            }
            // A short batch is the end of the range; a full one may or may not
            // be, so the walk continues and finds out.
            let Some(last) = last.filter(|_| batch.len() >= RANGE_SCAN_BATCH_ENTRIES) else {
                break;
            };
            from = resuming_after(last);
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

    /// The records an index holds, **greatest value first**, stopping once the
    /// bound is filled and its tie group closed.
    ///
    /// `None` when the index runs out before `wanted` records were found. That
    /// is not an error and not an empty answer: the records an index does not
    /// hold — the ones whose indexed value is absent — sort *below* every value
    /// it does hold, so an answer the index cannot fill needs them, and finding
    /// them is the scan. The caller falls back to it.
    ///
    /// # Why the tie group is drained
    ///
    /// The order a read answers in is the value system's order with ties broken
    /// by the record's identity **ascending**, and an entry's key is its value
    /// followed by that identity — so walking backwards yields a tie group with
    /// its identities *descending*. Cutting the walk at the bound would therefore
    /// take the wrong members of the group straddling it: ten records sharing one
    /// value under `LIMIT 10` would answer with the ten largest identities where
    /// the order asks for the ten smallest.
    ///
    /// So the walk continues past the bound until an entry carries a different
    /// value, and the caller sorts and cuts what comes back. Two properties make
    /// the comparison exact rather than approximate: the encoding is
    /// order-preserving, and it **normalises** — `1` and `1.0` encode
    /// identically, which is the same pair the value system's order calls equal.
    /// A tie group in bytes is a tie group in the order.
    ///
    /// The walk is unbounded only when the ordering is: a table whose every
    /// record carries one value costs the whole index, which is what ordering by
    /// a constant is.
    ///
    /// # Entries are taken at face value here, and that is a precondition
    ///
    /// Every other index read in this type treats an entry as a candidate and
    /// confirms it against the record, because entries hold the current state and
    /// carry no version. This one does not, because it has no condition to
    /// confirm against — the entry's *position* is the answer. Its caller
    /// therefore serves an ordering only from the committed tail, where every
    /// entry does reflect the record it points at.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn records_in_descending_order(
        &self,
        index: &IndexDefinition,
        wanted: usize,
    ) -> Result<Option<Vec<StoredRecord>>> {
        if wanted == 0 {
            return Ok(Some(Vec::new()));
        }
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let lower = Key::from(prefix.clone());
        let mut upper = Key::from(after(prefix));
        let mut found: Vec<StoredRecord> = Vec::new();
        // The value of the `wanted`-th record, once there is one. From then on
        // the walk is draining a tie group rather than filling a bound.
        let mut boundary: Option<IndexValues> = None;
        loop {
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::between(lower.clone(), upper.clone()),
                direction: ScanDirection::Reverse,
                limit: Some(DESCENDING_SCAN_BATCH_ENTRIES),
            };
            let batch = self.store.backend().scan(&request)?;
            let last = batch.last().map(|(key, _)| key.clone());
            // Decoded before anything is resolved, because deciding *what* to
            // resolve reads the entry's key and never its record: the boundary
            // test asks whether an entry is still in the tie group, and the
            // group is the value the key carries. So the walk can plan a whole
            // batch's reads without issuing one.
            let mut entries: Vec<(IndexValues, RecordId)> = Vec::with_capacity(batch.len());
            for (key, value) in &batch {
                entries.push(if index.unique {
                    (
                        UniqueIndexKey::decode(key.as_slice())?.values,
                        IndexTarget::decode(value.as_slice())?.id,
                    )
                } else {
                    let entry = SecondaryIndexKey::decode(key.as_slice())?;
                    (entry.values, entry.id)
                });
            }
            let mut at = 0;
            while at < entries.len() {
                // Sized to what is still needed rather than to the scan batch.
                // Resolving the whole batch would be one round trip instead of
                // ten and a hundred and twenty-eight record reads instead of
                // ten — a different cost, not a smaller one. At most one chunk
                // of overhang is read past the point the bound fills, and that
                // is bounded by `wanted`.
                let still = wanted.saturating_sub(found.len()).max(1);
                let mut end = at.saturating_add(still).min(entries.len());
                if let Some(edge) = &boundary {
                    // Draining a tie group, not filling a bound. Where it ends
                    // is knowable from the keys, so the drain reads exactly the
                    // records still in the group and stops.
                    end = at.saturating_add(
                        entries
                            .get(at..end)
                            .unwrap_or_default()
                            .iter()
                            .take_while(|(values, _)| values == edge)
                            .count(),
                    );
                    if end == at {
                        return Ok(Some(found));
                    }
                }
                let chunk = entries.get(at..end).unwrap_or_default();
                let addresses: Vec<RecordAddress> = chunk
                    .iter()
                    .map(|(_, id)| {
                        RecordAddress::new(index.namespace, index.database, index.table, id.clone())
                    })
                    .collect();
                for ((values, id), payload) in chunk.iter().zip(self.get_each(&addresses)?) {
                    if boundary.as_ref().is_some_and(|edge| edge != values) {
                        return Ok(Some(found));
                    }
                    if let Some(payload) = payload {
                        found.push((id.clone(), payload));
                        if found.len() >= wanted && boundary.is_none() {
                            boundary = Some(values.clone());
                        }
                    }
                }
                at = end;
            }
            // A short batch is the end of the index: the walk has seen every
            // entry, and whether that filled the bound is the whole answer.
            let Some(last) = last.filter(|_| batch.len() >= DESCENDING_SCAN_BATCH_ENTRIES) else {
                return Ok((found.len() >= wanted).then_some(found));
            };
            // The upper end is exclusive, so the next batch continues strictly
            // below the last entry this one read.
            upper = last;
        }
    }

    /// Whether this transaction has written to one table without committing.
    ///
    /// Asked by a read that would otherwise be served from an index: an
    /// uncommitted record has no entry, because entries are derived at commit,
    /// so an ordering served from the index would place it nowhere.
    #[must_use]
    pub fn writes_in(&self, namespace: NamespaceId, database: DatabaseId, table: TableId) -> bool {
        self.writes.keys().any(|address| {
            address.namespace == namespace && address.database == database && address.table == table
        })
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
