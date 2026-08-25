//! Reading a table without an index.
//!
//! The batched walk every other read is measured against: it costs the table,
//! and it is what the planner falls back to when no access path serves the
//! predicate.

use std::collections::BTreeMap;
use std::ops::Bound;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{RecordKey, RecordValue, StoreKey, StoreValue};
use tessari_kv::{Key, KeyRange, Keyspace, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use super::address::{after, resuming_after};
use super::{RecordAddress, Transaction};
use crate::error::Result;

impl Transaction<'_> {
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

    /// One batched scan of a span, up to `limit` entries.
    ///
    /// The batching is the backend's request limit rather than the caller's, so a
    /// caller asking for everything does not ask for it in one allocation.
    pub(super) fn scan_once(
        &self,
        keyspace: Keyspace,
        span: KeyRange,
        limit: usize,
    ) -> Result<Vec<(Key, tessari_kv::Value)>> {
        let mut taken = Vec::new();
        let mut from = match span.start() {
            Bound::Included(key) => key.as_slice().to_vec(),
            Bound::Excluded(key) => resuming_after(key.as_slice().to_vec()),
            Bound::Unbounded => Vec::new(),
        };
        while taken.len() < limit {
            let batch = self.store.backend().scan(&ScanRequest {
                keyspace,
                range: KeyRange::from_bounds(Bound::Included(Key::from(from)), span.end().clone()),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES.min(limit.saturating_sub(taken.len()))),
            })?;
            let full = batch.len() >= RANGE_SCAN_BATCH_ENTRIES;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            taken.extend(batch);
            let Some(last) = last.filter(|_| full) else {
                break;
            };
            from = resuming_after(last);
        }
        Ok(taken)
    }
}
