//! Reading a table without an index.
//!
//! The batched walk every other read is measured against: it costs the table,
//! and it is what the planner falls back to when no access path serves the
//! predicate.

use std::collections::BTreeMap;
use std::ops::{Bound, ControlFlow};

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
        self.table_records(namespace, database, table, None, None)
    }

    /// The live records of one table whose identity sorts after `anchor`.
    ///
    /// The seek behind a cursor. A record's key is its table prefix followed by
    /// its identity, so "after this record" is a **position in the keyspace**
    /// and not a predicate: the walk starts past the anchor's own versions and
    /// the records before it are never read at all. That is the whole difference
    /// between a cursor and an offset, and it is why this is a method here
    /// rather than a filter above.
    ///
    /// The anchor itself need not exist. It names a position, and a position is
    /// well defined whether or not something sits on it — which is what lets a
    /// page walk survive the deletion of the record it resumed from.
    ///
    /// `bound` carries the same looser-than-it-looks contract as
    /// [`Self::first_records_of`]: at least that many records, or every one
    /// after the anchor when there are fewer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn records_after(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        anchor: &RecordId,
        bound: Option<usize>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.table_records(namespace, database, table, bound, Some(anchor))
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
        self.table_records(namespace, database, table, Some(wanted), None)
    }

    /// The live records of one table, all of them or the first `bound` of them,
    /// starting past `anchor` when a cursor named one.
    fn table_records(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        bound: Option<usize>,
        anchor: Option<&RecordId>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let of_this_table = |address: &RecordAddress| {
            address.namespace == namespace && address.database == database && address.table == table
        };
        // The anchor's own versions are behind the page, not in it, so the walk
        // begins past the last of them rather than at the first.
        let opening = anchor.map_or_else(
            || prefix.clone(),
            |anchor| {
                after(RecordKey::versions_prefix(
                    namespace, database, table, anchor,
                ))
            },
        );
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
        let mut from = opening;
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
            for (id, value) in self.settled(batch, &mut resolved)? {
                if matches!(value, RecordValue::Present(_)) {
                    present = present.saturating_add(1);
                }
                live.insert(id, value);
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
            // A pending write is folded in only where the committed walk would
            // have reached it. Without the second test a record written but not
            // yet committed would appear on a page it sorts before, which is the
            // one way a cursor could answer with a record it had already handed
            // the caller.
            if of_this_table(address) && anchor.is_none_or(|anchor| &address.id > anchor) {
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

    /// Which records a batch of raw entries settles, in the order it holds them.
    ///
    /// Versions of one record are adjacent and sort newest-first, so the first
    /// entry at or before the snapshot is the visible one and every later entry
    /// for that record is an older version to walk past. `resolved` is the
    /// caller's because a record's versions may straddle a batch boundary, and
    /// forgetting which record was just settled would let an older version of it
    /// be read as a newer record.
    ///
    /// Asked by both the collecting walk and the streaming one. The two answer
    /// different shapes and must not disagree about which version a reader sees;
    /// two copies of that rule would be two places for it to drift.
    fn settled(
        &self,
        batch: Vec<(Key, tessari_kv::Value)>,
        resolved: &mut Option<RecordId>,
    ) -> Result<Vec<(RecordId, RecordValue)>> {
        let mut taken = Vec::with_capacity(batch.len());
        for (key, value) in batch {
            let decoded = RecordKey::decode(key.as_slice())?;
            if decoded.version > self.snapshot || resolved.as_ref() == Some(&decoded.id) {
                continue;
            }
            *resolved = Some(decoded.id.clone());
            taken.push((decoded.id, RecordValue::decode(value.as_slice())?));
        }
        Ok(taken)
    }

    /// One batch of raw entries from a span of the record keyspace.
    ///
    /// Its own method so that a caller whose error type is not this crate's can
    /// still write `?` over the scan: everything fallible about the walk is on
    /// this side of the boundary, and the callback's side converts once.
    fn batch_of(
        &self,
        from: &[u8],
        end: &[u8],
        limit: Option<usize>,
    ) -> Result<Vec<(Key, tessari_kv::Value)>> {
        Ok(self.store.backend().scan(&ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::between(Key::from(from.to_vec()), Key::from(end.to_vec())),
            direction: ScanDirection::Forward,
            limit,
        })?)
    }

    /// Every live record of one table, handed over as the walk finds them.
    ///
    /// The streaming twin of [`Self::scan_table`], and one difference is the
    /// whole of it: `hand` may answer `Break`, and the walk then stops where it
    /// stands instead of after the table.
    ///
    /// # Why this exists beside a method that already reads a table
    ///
    /// A read whose answer count is its `LIMIT` pushes that bound into the
    /// source (ADR-0013) and costs the bound. A read with a `WHERE` cannot: the
    /// bound counts records that **match** and the source counts records that
    /// **exist**, so no number can be handed down. What the caller has instead
    /// is the consumer's `Break`, which every stage above the source already
    /// honours — and which `scan_table` cannot deliver, because it builds the
    /// whole table's payloads before the caller evaluates its first predicate.
    /// Measured before this was written: a match found at the third of a hundred
    /// thousand records cost 83.3 ms, a match at the last cost 82.9, and no
    /// match at all cost 84.3. The position of the match did not change the
    /// cost, which is what a source that cannot be stopped looks like.
    ///
    /// The **answer** is still a materialised value, so ADR-0013's refusal of
    /// streaming stands untouched: nothing is handed to a caller while a
    /// snapshot is open, and the snapshot's life gets shorter rather than longer
    /// because the source stops. What streams is the source's own buffering, one
    /// level below the answer, exactly as ADR-0014 already did for decoding.
    ///
    /// # What `hand` receives
    ///
    /// The transaction itself, because a caller that stops early is deciding
    /// something — testing a condition, feeding a consumer — and both need it.
    /// Records arrive in key order with this transaction's own uncommitted
    /// writes merged into that order, deleted records left out, and a record
    /// this transaction has written answered from the write rather than from the
    /// committed version underneath it.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, when stored bytes cannot be
    /// decoded, or when `hand` itself fails.
    pub fn walk_table<F, E>(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        mut hand: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(&mut Self, RecordId, Vec<u8>) -> std::result::Result<ControlFlow<()>, E>,
        E: From<crate::error::Error>,
    {
        let prefix = RecordKey::table_prefix(namespace, database, table);
        // Copied out before the walk rather than read in step with it: `hand`
        // takes the transaction, so nothing may hold a borrow of it across the
        // call. There are as many of these as this transaction has written to
        // this table, which for the read that motivates this walk is none.
        let mut pending = self
            .writes
            .iter()
            .filter(|(address, _)| {
                address.namespace == namespace
                    && address.database == database
                    && address.table == table
            })
            .map(|(address, value)| (address.id.clone(), value.clone()))
            .collect::<Vec<_>>()
            .into_iter()
            .peekable();

        let mut resolved: Option<RecordId> = None;
        let mut from = prefix.clone();
        let end = after(prefix);
        loop {
            // Batched although the walk names no bound. `table_records` asks for
            // everything in one request because it is going to hold everything
            // anyway; here a single unbounded request would read the table
            // before the first record could ask to stop, which is the cost this
            // walk exists to remove.
            let batch = self.batch_of(&from, &end, Some(RANGE_SCAN_BATCH_ENTRIES))?;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            let entries = batch.len();
            for (id, value) in self.settled(batch, &mut resolved)? {
                // Pending writes sorting before this record come first, so the
                // records arrive in key order whether they are committed or not
                // — the order `scan_table` answers in, and therefore the order a
                // bound above truncates.
                while let Some((waiting, written)) = pending.next_if(|(waiting, _)| waiting < &id) {
                    if let RecordValue::Present(payload) = written
                        && hand(self, waiting, payload)?.is_break()
                    {
                        return Ok(());
                    }
                }
                // A record this transaction has written is answered from the
                // write and not from the committed version beneath it —
                // including when the write is a tombstone, which removes it.
                let value = match pending.next_if(|(waiting, _)| waiting == &id) {
                    Some((_, written)) => written,
                    None => value,
                };
                if let RecordValue::Present(payload) = value
                    && hand(self, id, payload)?.is_break()
                {
                    return Ok(());
                }
            }
            // A batch shorter than the one asked for is the end of the table.
            if entries < RANGE_SCAN_BATCH_ENTRIES {
                break;
            }
            let Some(last) = last else {
                break;
            };
            from = resuming_after(last);
        }
        // Whatever the committed walk never reached: records written in this
        // transaction that sort after the last one on disk.
        for (waiting, written) in pending {
            if let RecordValue::Present(payload) = written
                && hand(self, waiting, payload)?.is_break()
            {
                return Ok(());
            }
        }
        Ok(())
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
