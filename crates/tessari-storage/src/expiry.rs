//! Removing what a series table has stopped answering with.
//!
//! The floor hides a record; this removes it. They are deliberately two acts —
//! that separation is the engine's safety property, because correctness comes
//! from the read and a pass that lags, is throttled or has never run costs
//! storage rather than an answer.
//!
//! # A range, not a record at a time (ADR-0088 §7, G044 C11)
//!
//! Everything below the floor is one contiguous span of the table's keys,
//! because a series is keyed by time. So the pass removes it as one range delete
//! — one write whatever the span holds — rather than one tombstone, one log
//! entry and one commit share per record. The bytes are gone once the engine
//! compacts the range, with no version reclamation to wait for.
//!
//! # It is a reclamation, and so it is not on the log or the feed
//!
//! The answer changed when the floor passed, not when this runs, so the pass
//! changes no answer and a follower or a subscriber has nothing to learn from
//! it. Like [`Store::reclaim_table`] it is this node's own storage work: every
//! node runs it over its own copy, and reaches the same span because the floor
//! is a function of the clock and the key. A consumer mirroring a series table
//! applies the same retention on its side.
//!
//! # Two things it must not get wrong
//!
//! It judges the floor at the instant the **oldest live reader** began, not now:
//! a transaction begun earlier reads at an earlier floor, and a range taken at
//! today's would remove records it still answers with. And it takes a removed
//! record's **index entries** first, in bounded batches, and the records after:
//! a crash between the two leaves records past the floor with no entries — hidden
//! either way, and removed by the next run — where the other order would leave
//! entries naming records that no longer exist.

use std::time::{SystemTime, UNIX_EPOCH};

use tessari_encoding::{
    LogRecord, Mutation, RecordKey, RecordValue, StampedValue, StoreKey, StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use crate::catalog::{Catalog, TableKind};
use crate::error::Result;
use crate::store::Store;

/// How many records' index entries are taken out in one batch.
const BATCH_RECORDS: usize = 512;

/// What one pass over one table removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Expired {
    /// Range deletes issued: one when anything was below the floor, else none.
    pub ranges: usize,
    /// Records whose index entries were taken out before the range went.
    pub indexed: usize,
}

impl Store {
    /// Remove the records one series table has stopped answering with.
    ///
    /// Answers `Expired::default()` for a table that is not a series, or one with
    /// nothing below its floor, so a caller sweeping a database does not have to
    /// ask what each table is.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, a stored key or value cannot be
    /// decoded, or the catalog cannot be read.
    pub fn expire_series(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Expired> {
        // The wall clock first and the age second, so the instant is at or
        // before the moment the oldest reader registered, never after it.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let held_back = self.oldest_snapshot_age().map_or(0, |age| {
            u64::try_from(age.as_millis()).map_or(u64::MAX, |millis| millis.saturating_add(1))
        });
        let mut view = self.begin()?;
        let Some(floor) = view.series_floor_at(namespace, table, now.saturating_sub(held_back))?
        else {
            return Ok(Expired::default());
        };
        let start: Key = RecordKey::table_prefix(namespace, database, table).into();
        let end: Key = RecordKey::versions_prefix(namespace, database, table, &floor).into();
        let range = KeyRange::between(start.clone(), end.clone());
        let first = self.backend().scan(&ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: range.clone(),
            direction: ScanDirection::Forward,
            limit: Some(1),
        })?;
        if first.is_empty() {
            return Ok(Expired::default());
        }
        let indexed = if Catalog::new(&mut view)
            .indexes_on(table)?
            .iter()
            .any(|index| index.namespace == namespace && index.database == database)
        {
            self.unindex_below(namespace, database, table, start, &end)?
        } else {
            0
        };
        self.backend().delete_range(RecordKey::keyspace(), &range)?;
        Ok(Expired { ranges: 1, indexed })
    }

    /// [`Self::expire_series`] over every series table in the store, for the
    /// node's housekeeping cadence.
    ///
    /// # Errors
    ///
    /// The first failure of any table's pass, or of reading the catalog.
    pub fn expire_every_series(&self) -> Result<Expired> {
        let mut series = Vec::new();
        {
            let mut transaction = self.begin()?;
            let catalog = Catalog::new(&mut transaction);
            for namespace in catalog.namespaces()? {
                for database in catalog.databases_in(namespace.id)? {
                    for table in catalog.tables_in(namespace.id, database.id)? {
                        if matches!(table.kind, TableKind::Series(_)) {
                            series.push((table.namespace, table.database, table.id));
                        }
                    }
                }
            }
        }
        let mut total = Expired::default();
        for (namespace, database, table) in series {
            let expired = self.expire_series(namespace, database, table)?;
            total.ranges = total.ranges.saturating_add(expired.ranges);
            total.indexed = total.indexed.saturating_add(expired.indexed);
        }
        Ok(total)
    }

    /// Take out the index entries of every record present in `[start, end)`, a batch
    /// of records at a time, through the same index code a commit uses.
    ///
    /// Answers how many records that was.
    fn unindex_below(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        start: Key,
        end: &Key,
    ) -> Result<usize> {
        let mut unindexed = 0_usize;
        let mut from = KeyRange::between(start, end.clone());
        loop {
            let pairs = self.backend().sweep(&ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: from.clone(),
                direction: ScanDirection::Forward,
                limit: Some(BATCH_RECORDS),
            })?;
            let Some((last, _)) = pairs.last() else {
                return Ok(unindexed);
            };
            // Past the last key read, so the next batch starts where this ended
            // rather than at the start of the range again.
            let mut next = last.as_slice().to_vec();
            next.push(0);
            from = KeyRange::between(Key::from_slice(&next), end.clone());

            // Versions of one record are adjacent and sort newest-first, so the
            // first entry for an identity says whether it is still there.
            let mut seen: Option<RecordId> = None;
            let mut mutations = Vec::new();
            for (key, value) in &pairs {
                let decoded = RecordKey::decode(key.as_slice())?;
                if seen.as_ref() == Some(&decoded.id) {
                    continue;
                }
                let stored = StampedValue::decode(value.as_slice())?;
                // An intent says nothing about whether the record is there; the
                // version under it, next in this walk, does (ADR-0112 D5).
                if crate::intents::is_intent(&stored) {
                    continue;
                }
                seen = Some(decoded.id.clone());
                if matches!(stored.into_value(), RecordValue::Present(_)) {
                    mutations.push(Mutation {
                        namespace,
                        database,
                        table,
                        id: decoded.id,
                        shard: None,
                        value: StampedValue::new(RecordValue::Tombstone),
                    });
                }
            }
            if mutations.is_empty() {
                continue;
            }
            unindexed = unindexed.saturating_add(mutations.len());
            let view = self.begin()?;
            view.lift_floor(table);
            let batch = crate::index::maintain_from(
                self,
                view,
                &LogRecord::new(mutations),
                WriteBatch::new(),
            )?;
            self.backend().apply(batch)?;
        }
    }
}
