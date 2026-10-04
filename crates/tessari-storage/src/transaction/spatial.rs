//! Reads over the cells a geometry covers.
//!
//! Both reads here are filter-and-refine: the index answers with candidates
//! whose stored box overlaps, and the exact predicate decides. The counts they
//! carry are the health metric of that arrangement.

use std::collections::BTreeMap;
use std::ops::Bound;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    IndexAddress, KeyKind, RecordValue, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::RecordId;

use super::address::resuming_after;
use super::{RecordAddress, StoredRecord, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

/// What a spatial filter read, and what survived it.
///
/// The counts are not decoration. The candidate-to-result ratio is the health
/// metric of a spatial index — a ratio near one means the stored boxes
/// approximate their geometries well, and a large one means the index is doing
/// work that the exact predicate throws away. Without it, a query budget is
/// tuned by intuition, and a structurally bad row — a river, a road, a border,
/// whose box is many times its own area — is invisible.
///
/// Three numbers rather than one, because they fail differently: `entries` is
/// the traversal's own cost, `reached` is how many distinct records that was,
/// and `candidates` is how many the boxes could not rule out.
#[derive(Debug, Clone)]
pub struct Region {
    /// The records to test, with their stored bytes.
    pub rows: Vec<StoredRecord>,
    /// How many index entries the traversal read.
    pub entries: usize,
    /// How many distinct records those entries named.
    pub reached: usize,
    /// How many of those the box test admitted.
    pub candidates: usize,
}

impl Transaction<'_> {
    /// The records a spatial index offers for a query box.
    ///
    /// # Filter, and only filter
    ///
    /// This is the **filter** half of filter-and-refine and nothing more. It
    /// answers with a superset of the records that can satisfy the predicate,
    /// having rejected the ones their own stored box already settles. The exact
    /// predicate runs above it, on the real geometry, the way the whole
    /// condition is re-tested above every other index read in this store — so a
    /// candidate that turns out not to match costs work and never an answer.
    ///
    /// # Both halves of the traversal, and why neither is optional
    ///
    /// Each cell of the query's covering contributes two reads:
    ///
    /// - one **scan** of everything at or below it, because a cell's descendants
    ///   are one contiguous span of the key space;
    /// - one **lookup** per level above it, because a record **larger** than the
    ///   query box sits at a coarser cell whose range begins *below* the query
    ///   cell's own, where no forward scan will ever reach it.
    ///
    /// Dropping the second half leaves a store that answers small queries
    /// correctly and loses exactly the large records — fewer rows, no error, and
    /// the only symptom is that somebody's country is missing from a search for
    /// a street in it.
    ///
    /// # Why a record is only judged once
    ///
    /// A record has one entry per cell of its own covering, and every one of
    /// them carries the same box. Several query cells may reach the same record.
    /// So the box test runs once per record rather than once per entry, and the
    /// counts returned separate the two — `entries` is what the traversal read,
    /// `reached` how many records that was, and `candidates` how many survived.
    /// Their ratios are the health of the index and are what a tuning decision
    /// has to be made from rather than guessed at.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key or box cannot be decoded.
    pub fn records_in_region(
        &self,
        index: &IndexDefinition,
        cells: &[tessari_geo::Cell],
        query: tessari_geo::Bounds,
        relation: tessari_geo::Relation,
    ) -> Result<Region> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let keyspace = KeyKind::SpatialIndex.keyspace();

        // Whether each record reached survived its own box test. A map rather
        // than two sets, so a record reached again through another cell is not
        // judged a second time and cannot be judged differently.
        let mut judged: BTreeMap<RecordId, bool> = BTreeMap::new();
        let mut entries = 0_usize;

        for cell in cells {
            let mut spans = vec![SpatialIndexKey::descendants(&address, *cell)];
            for level in 0..cell.level() {
                if let Some(above) = cell.ancestor(level) {
                    spans.push(KeyRange::prefix(&SpatialIndexKey::cell_prefix(
                        &address, above,
                    )));
                }
            }
            for span in spans {
                let mut from = match span.start() {
                    Bound::Included(key) => key.as_slice().to_vec(),
                    Bound::Excluded(key) => resuming_after(key.as_slice().to_vec()),
                    Bound::Unbounded => Vec::new(),
                };
                loop {
                    let request = ScanRequest {
                        keyspace,
                        range: KeyRange::from_bounds(
                            Bound::Included(Key::from(from)),
                            span.end().clone(),
                        ),
                        direction: ScanDirection::Forward,
                        limit: Some(RANGE_SCAN_BATCH_ENTRIES),
                    };
                    let batch = self.store.backend().scan(&request)?;
                    let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
                    for (key, value) in &batch {
                        entries = entries.saturating_add(1);
                        let id = SpatialIndexKey::decode(key.as_slice())?.id;
                        // Occupied means this record was reached through another
                        // cell already, and its verdict does not depend on which
                        // one — every entry of a record carries the record's own
                        // box.
                        if let std::collections::btree_map::Entry::Vacant(slot) = judged.entry(id) {
                            let bounds = SpatialExtent::decode(value.as_slice())?.bounds;
                            slot.insert(relation.admits(query, bounds));
                        }
                    }
                    let Some(last) = last.filter(|_| batch.len() >= RANGE_SCAN_BATCH_ENTRIES)
                    else {
                        break;
                    };
                    from = resuming_after(last);
                }
            }
        }

        let reached = judged.len();
        let admitted: Vec<RecordId> = judged
            .into_iter()
            .filter_map(|(id, kept)| kept.then_some(id))
            .collect();
        let candidates = admitted.len();

        let addresses: Vec<RecordAddress> = admitted
            .into_iter()
            .map(|id| RecordAddress::new(index.namespace, index.database, index.table, id))
            .collect();
        let mut found: BTreeMap<RecordId, Vec<u8>> = BTreeMap::new();
        for (address, payload) in addresses.iter().zip(self.get_each(&addresses)?) {
            if let Some(payload) = payload {
                found.insert(address.id.clone(), payload);
            }
        }

        // A record this transaction wrote but has not committed has no index
        // entry yet, so it has no box to be filtered by either. It is offered
        // unconditionally: the condition above decides, and a candidate that
        // does not match costs work while a record never offered is a missing
        // row. Every other index read in this store folds pending writes the
        // same way.
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

        Ok(Region {
            rows: found.into_iter().collect(),
            entries,
            reached,
            candidates,
        })
    }
}
