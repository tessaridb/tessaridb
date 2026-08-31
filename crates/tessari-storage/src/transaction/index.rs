//! Reads served by a secondary, range or vector index.
//!
//! Each of these turns a predicate into an index range rather than a table
//! walk. The vector read is the only approximate one in the module, and says so
//! where it is declared.

use std::collections::BTreeMap;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    IndexAddress, IndexTarget, IndexValues, KeyKind, RecordValue, SecondaryIndexKey, StoreKey,
    StoreValue, VectorRecall, VectorRecallKey,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value};

use super::address::{after, resuming_after};
use super::{RecordAddress, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

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
    /// `effort` is the walk's budget: `None` for the engine's own, `Some` for a
    /// budget the read named with `APPROXIMATE EFFORT n`.
    pub fn records_by_vector(
        &self,
        index: &IndexDefinition,
        query: &[f64],
        wanted: usize,
        effort: Option<usize>,
    ) -> Result<Vec<RecordId>> {
        let Some(distance) = index.vector else {
            return Ok(Vec::new());
        };
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let graph = crate::graph::Graph::read(self.store, &address, distance)?;
        Ok(graph.nearest(query, wanted, effort))
    }

    /// The recall this vector index was last measured at, if it ever was.
    ///
    /// `None` means nobody has measured — an index is measured when it is built,
    /// so a store filled by writes since its last build reports the figure from
    /// that build, and one never built reports nothing. That is the honest
    /// answer and the reason the figure carries `records`: a reader can see the
    /// store has outgrown the number.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn vector_recall(&self, index: &IndexDefinition) -> Result<Option<VectorRecall>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let key = VectorRecallKey::new(address).encode();
        match self
            .store
            .backend()
            .get(VectorRecallKey::keyspace(), &key)?
        {
            Some(bytes) => Ok(Some(VectorRecall::decode(bytes.as_slice())?)),
            None => Ok(None),
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
}
