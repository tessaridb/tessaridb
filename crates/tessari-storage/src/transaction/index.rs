//! Reads served by a secondary, range or vector index.
//!
//! Each of these turns a predicate into an index range rather than a table
//! walk. The vector read is the only approximate one in the module, and says so
//! where it is declared.

mod equality;
mod vectors;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, RecordValue, SecondaryIndexKey, StoreKey,
    StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value};

use super::address::{after, resuming_after};
use super::{RecordAddress, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

/// The first batch a streamed walk reads, before it doubles.
///
/// The fetch batch **ramps**, and a fixed one would have made a streamed walk
/// pointless. Records are read a batch at a time so that a wide answer costs one
/// round trip per batch rather than one per record; but a batch of
/// `RANGE_SCAN_BATCH_ENTRIES` is read in full before its first record is handed
/// over, so a caller wanting ten of four hundred candidates still paid for four
/// hundred and the walk saved nothing.
///
/// Doubling from a small first batch settles it in both directions. A bound that
/// fills early pays one short batch; a read that wants everything reaches the
/// full batch size after seven of them and from there costs what it always did —
/// reaching a hundred thousand records takes about five more round trips than a
/// fixed batch would.
const FIRST_FETCH_BATCH: usize = 8;

impl Transaction<'_> {
    /// The records an ordered index holds between two bounds, inside the run its
    /// `fixed` leading values name.
    ///
    /// # What `fixed` is for
    ///
    /// An empty `fixed` is a range on the index's **leading** field, which is
    /// every range this store served before composite ranges existed. A
    /// non-empty one names a value for each field before the ranged one, so
    /// `(at, tag)` under `at = 20 AND tag >= 1950` walks only the entries of that
    /// day. The bytes are built the same way in both cases — the fixed values and
    /// the bound are one run of encoded values — so with `fixed` empty every key
    /// this builds is byte-identical to the ones it built before, which is what
    /// makes the existing range reads the regression proof for the general one.
    ///
    /// The caller is responsible for `fixed` being a genuine leading run of the
    /// index's fields with the ranged field immediately after it; `plan::ranged`
    /// is the only thing that decides that, and it stops at the first field no
    /// equality fixes.
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
    /// than this read. `docs/tessariql.md` §8 records that with the numbers.
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
        fixed: &[Value],
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
        // The fixed values and the bound are one run of encoded values, so a
        // bound is appended to the fixed run rather than encoded beside it.
        let within = |bound: Option<&Value>| {
            let mut values = fixed.to_vec();
            if let Some(held) = bound {
                values.push(held.clone());
            }
            let mut bytes = prefix.clone();
            bytes.extend_from_slice(&IndexValues::leading(&values));
            bytes
        };
        // An absent bound is unbounded within the fixed run, not within the whole
        // index — which for an empty run is the same thing, and for a non-empty
        // one is the difference between reading a day and reading the table.
        let start = within(lower);
        // The end is exclusive in `KeyRange::between`, and the bound itself must
        // be included — so the stop point is one byte past every key that begins
        // with the bound's encoding.
        let end = after(within(upper));

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

    /// The identities an ordered index holds between two bounds, in record
    /// order, with this transaction's own writes folded in.
    ///
    /// [`Self::records_in_range`] without the payloads. The entry walk and the
    /// pending-write fold are the same; what is left out is the `get_each` per
    /// batch, which is the half whose cost grows with the answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or an index entry cannot be
    /// decoded.
    pub fn ids_in_range(
        &self,
        index: &IndexDefinition,
        fixed: &[Value],
        lower: Option<&Value>,
        upper: Option<&Value>,
    ) -> Result<BTreeSet<RecordId>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let within = |bound: Option<&Value>| {
            let mut values = fixed.to_vec();
            if let Some(held) = bound {
                values.push(held.clone());
            }
            let mut bytes = prefix.clone();
            bytes.extend_from_slice(&IndexValues::leading(&values));
            bytes
        };
        let start = within(lower);
        let end = after(within(upper));

        let mut found: BTreeSet<RecordId> = BTreeSet::new();
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
            for (key, value) in &batch {
                let id = if index.unique {
                    IndexTarget::decode(value.as_slice())?.id
                } else {
                    SecondaryIndexKey::decode(key.as_slice())?.id
                };
                found.insert(id);
            }
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
                RecordValue::Present(_) => {
                    found.insert(address.id.clone());
                }
                RecordValue::Tombstone => {
                    found.remove(&address.id);
                }
            }
        }
        Ok(found)
    }

    /// How many entries an ordered index holds between two bounds, giving up
    /// once there are more than `cap` of them.
    ///
    /// `None` means "more than `cap`" and is the whole point of the function.
    /// The planner asks this to find out whether a range is selective enough to
    /// be worth serving by the index; a range that is not selective is exactly
    /// the case where counting it to the end would cost as much as the scan the
    /// count exists to avoid. So the walk stops, and the caller learns the one
    /// fact it needed — that this candidate is not better than reading the
    /// table.
    ///
    /// It counts **entries**, not records, and it does not fold this
    /// transaction's pending writes. Entries are an upper bound on records (a
    /// multi-valued field puts a record under several of them), which is what
    /// `AtMost` means, and the estimate decides an access path rather than an
    /// answer — see [`crate::catalog::Catalog::record_count`] for why nothing
    /// that decides which records a statement returns may read a number like
    /// this one.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails.
    pub fn count_in_range(
        &self,
        index: &IndexDefinition,
        fixed: &[Value],
        lower: Option<&Value>,
        upper: Option<&Value>,
        cap: u64,
    ) -> Result<Option<u64>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let within = |bound: Option<&Value>| {
            let mut values = fixed.to_vec();
            if let Some(held) = bound {
                values.push(held.clone());
            }
            let mut bytes = prefix.clone();
            bytes.extend_from_slice(&IndexValues::leading(&values));
            bytes
        };
        let end = after(within(upper));

        let mut found = 0_u64;
        let mut from = within(lower);
        loop {
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::between(Key::from(from), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES),
            };
            let batch = self.store.backend().scan(&request)?;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            found = found.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
            if found > cap {
                return Ok(None);
            }
            let Some(last) = last.filter(|_| batch.len() >= RANGE_SCAN_BATCH_ENTRIES) else {
                break;
            };
            from = resuming_after(last);
        }
        Ok(Some(found))
    }

    /// The records an ordered index names between two bounds, handed over one at
    /// a time in record order, stopping where the caller says to stop.
    ///
    /// The streaming twin of [`Self::records_in_range`], and it streams **half**
    /// of that read rather than all of it. The entry walk still runs to the end,
    /// because the answer is in record order and the lowest identity among the
    /// candidates cannot be known until every candidate has been named — so a
    /// walk that stopped early would answer with whichever records the index
    /// happened to reach first, which is a different answer from the one a scan
    /// gives for the same predicate. That equality is the invariant an index
    /// exists under: it narrows, and it never changes what a query returns.
    ///
    /// What the bound does reach is the **fetch**, which is the half whose cost
    /// grows with the answer: entries are named in one pass, then records are
    /// read in identity order a batch at a time, and a caller that has filled
    /// its bound stops the next batch from being read at all. Batched rather
    /// than one at a time because a point get per record is a backend round trip
    /// per record, which is the cost `get_each` exists to avoid.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, an entry cannot be decoded, or
    /// `hand` returns one.
    pub fn walk_records_in_range<F, E>(
        &mut self,
        index: &IndexDefinition,
        fixed: &[Value],
        lower: Option<&Value>,
        upper: Option<&Value>,
        mut hand: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(&mut Self, RecordId, Vec<u8>) -> std::result::Result<ControlFlow<()>, E>,
        E: From<crate::error::Error>,
    {
        // A complete value of a secondary index is the one run whose entries are
        // already in record order, so it is walked and stopped rather than
        // collected (`index/equality.rs`).
        if !index.unique
            && lower.is_none()
            && upper.is_none()
            && !fixed.is_empty()
            && fixed.len() == index.fields.len()
        {
            return self.walk_complete_equality(index, fixed, hand);
        }
        let ids = self.ids_in_range(index, fixed, lower, upper)?;
        // Copied out before the walk rather than read in step with it: `hand`
        // takes the transaction, so nothing may hold a borrow of it across the
        // call. A pending write's payload is the one this transaction wrote, and
        // it wins over whatever the store still holds for that identity.
        let pending: BTreeMap<RecordId, Vec<u8>> = self
            .writes
            .iter()
            .filter(|(address, _)| {
                address.namespace == index.namespace
                    && address.database == index.database
                    && address.table == index.table
            })
            .filter_map(|(address, held)| match held {
                RecordValue::Present(payload) => Some((address.id.clone(), payload.clone())),
                RecordValue::Tombstone => None,
            })
            .collect();

        let ids: Vec<RecordId> = ids.into_iter().collect();
        let mut taken = 0_usize;
        let mut batch = FIRST_FETCH_BATCH;
        while taken < ids.len() {
            let upto = ids.len().min(taken.saturating_add(batch));
            let addresses = ids
                .get(taken..upto)
                .unwrap_or_default()
                .iter()
                .map(|id| {
                    RecordAddress::new(index.namespace, index.database, index.table, id.clone())
                })
                .collect::<Vec<_>>();
            let payloads = self.get_each(&addresses)?;
            for (address, stored) in addresses.into_iter().zip(payloads) {
                let Some(payload) = pending.get(&address.id).cloned().or(stored) else {
                    continue;
                };
                if hand(self, address.id, payload)?.is_break() {
                    return Ok(());
                }
            }
            taken = upto;
            batch = batch.saturating_mul(2).min(RANGE_SCAN_BATCH_ENTRIES);
        }
        Ok(())
    }
}
