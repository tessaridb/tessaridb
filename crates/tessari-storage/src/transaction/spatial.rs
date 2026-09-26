//! Reads over the cells a geometry covers.
//!
//! Both reads here are filter-and-refine: the index answers with candidates
//! whose stored box overlaps, and the exact predicate decides. The counts they
//! carry are the health metric of that arrangement.

use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Bound;

use tessari_constants::{RANGE_SCAN_BATCH_ENTRIES, SPATIAL_WALK_SUBTREE_ENTRIES};
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

/// A nearest-first answer, and what the walk that produced it cost.
#[derive(Debug, Clone, Default)]
pub struct Nearby {
    /// The records, nearest first, ties broken by identity.
    pub rows: Vec<StoredRecord>,
    /// How many index entries the walk read.
    pub entries: usize,
    /// How many cells it opened.
    pub expanded: usize,
}

/// A cell waiting to be opened, keyed by a distance nothing inside it can beat.
///
/// Ordered **backwards** deliberately: [`BinaryHeap`] hands back its greatest
/// element and this walk wants its cheapest cell, so the smallest bound has to
/// compare greatest. `total_cmp` rather than `partial_cmp` because a heap needs
/// a total order, and there is no useful answer to give if two bounds turn out
/// incomparable.
#[derive(Debug, Clone, Copy)]
struct Frontier {
    bound: f64,
    cell: tessari_geo::Cell,
}

impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.bound.total_cmp(&self.bound)
    }
}

impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Frontier {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Frontier {}

/// A record the walk has placed, ordered by distance with the **worst** at the
/// head — which is the threshold the stopping test reads.
#[derive(Debug, Clone)]
struct Ranked {
    metres: f64,
    id: RecordId,
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.metres
            .total_cmp(&other.metres)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Ranked {}

/// The true distance from `target` to a record whose stored box is a single
/// position, or `None` when the walk must give the read up to the scan.
///
/// A box with any extent belongs to a shape rather than a point, and the
/// distance to a shape is the distance to its nearest point — which the box does
/// not know and the index does not hold — so that record is the scan's to
/// measure. A pair the distance refuses
/// as near-antipodal is given up for a different reason: the value layer turns
/// that refusal into `none`, which sorts *below* every number, so ranking it
/// last would put it where the scan does not.
fn exactly_to(target: tessari_geo::Snapped, bounds: tessari_geo::Bounds) -> Option<f64> {
    if bounds.west() != bounds.east() || bounds.south() != bounds.north() {
        return None;
    }
    let at = tessari_geo::Snapped::from_units(bounds.west(), bounds.south()).ok()?;
    tessari_geo::distance(target, at)
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

    /// The records nearest to `target`, nearest first, taken from a spatial
    /// index by a best-first walk over cells.
    ///
    /// # The walk, and the one thing it rests on
    ///
    /// Cells wait in a queue keyed by [`tessari_geo::no_closer_than`] — a
    /// distance nothing inside the cell can beat. The cheapest waiting cell is
    /// opened next, and the walk stops when the cheapest one **left** is further
    /// away than the worst answer already held. That argument is only sound
    /// because the key is a floor: a key that could exceed the true distance to
    /// something inside the cell would let the walk discard the cell holding the
    /// nearest record and answer with the second.
    ///
    /// **The tie group is kept by the cut at the end, not by the stopping test.**
    /// Two records the same distance away are both in the answer or neither is,
    /// which is what a scan gives — and that comes from taking every candidate at
    /// or inside the `wanted`-th distance rather than the first `wanted` of them.
    /// The stopping test is a strict `>` because that is the correct rule, and it
    /// is deliberately *not* the thing holding the tie group up: with these two
    /// floors it cannot differ from `>=`, since both are strictly below any
    /// positive distance they bound — a meridian arc exceeds `M_min·Δφ` over any
    /// positive span, and a geodesic exceeds the chord to the wedge. Believing
    /// otherwise, and leaning the tie group on it, was a mistake the falsification
    /// pass caught by breaking the test and watching nothing happen.
    ///
    /// # Why a cell is read as a subtree before it is split
    ///
    /// The tree here is implicit: every cell exists at every level whether or not
    /// anything was written there, so descending blindly costs a seek per level
    /// all the way down. A cell is therefore first read whole, limited to one
    /// entry above [`SPATIAL_WALK_SUBTREE_ENTRIES`]; a short answer means nothing
    /// was truncated and the walk has the entire subtree in hand without
    /// descending at all.
    ///
    /// # `None` is the scan, and every one of them is a way the answers differ
    ///
    /// - **a record whose stored box is not a single position.** Only a point has
    ///   a degenerate box; the distance to a larger shape is to its nearest
    ///   point, which needs the geometry the index does not hold.
    /// - **a near-antipodal record.** [`tessari_geo::distance`] refuses those
    ///   rather than guessing, and the value layer above turns that refusal into
    ///   `none`, which sorts *below* every number. A walk that ranked it last
    ///   would put it exactly where the scan does not.
    /// - **the index ran out before the bound was filled**, which is the answer
    ///   needing records the index does not hold.
    ///
    /// The caller owns the other refusals — an uncommitted write, a snapshot
    /// behind the committed tail, a field the reader cannot see — because those
    /// are facts about the session rather than about the index.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or an entry cannot be decoded.
    pub fn records_by_place(
        &self,
        index: &IndexDefinition,
        target: tessari_geo::Snapped,
        wanted: usize,
    ) -> Result<Option<Nearby>> {
        if wanted == 0 {
            return Ok(Some(Nearby::default()));
        }
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let keyspace = KeyKind::SpatialIndex.keyspace();

        let mut frontier = BinaryHeap::new();
        frontier.push(Frontier {
            bound: 0.0,
            cell: tessari_geo::Cell::root(),
        });
        // Every record the walk has ranked, so an entry reached again through a
        // second cell of the same record's covering is ranked once.
        let mut ranked: BTreeMap<RecordId, f64> = BTreeMap::new();
        // The best `wanted` so far, worst at the head, which is the threshold the
        // stopping test compares against.
        let mut best: BinaryHeap<Ranked> = BinaryHeap::new();
        let mut entries = 0_usize;
        let mut expanded = 0_usize;

        while let Some(next) = frontier.pop() {
            if best.len() >= wanted
                && let Some(worst) = best.peek()
                && next.bound > worst.metres
            {
                break;
            }
            expanded = expanded.saturating_add(1);
            let subtree = self.scan_once(
                keyspace,
                SpatialIndexKey::descendants(&address, next.cell),
                SPATIAL_WALK_SUBTREE_ENTRIES.saturating_add(1),
            )?;
            let whole = subtree.len() <= SPATIAL_WALK_SUBTREE_ENTRIES;
            let read = if whole {
                subtree
            } else {
                // The subtree is too big to take at once, so only what is stored
                // at this exact cell is ranked here and the rest is left to the
                // four children, each of which the queue will place on its own
                // distance rather than on this one's.
                self.scan_once(
                    keyspace,
                    KeyRange::prefix(&SpatialIndexKey::cell_prefix(&address, next.cell)),
                    usize::MAX,
                )?
            };
            for (key, value) in &read {
                entries = entries.saturating_add(1);
                let id = SpatialIndexKey::decode(key.as_slice())?.id;
                if ranked.contains_key(&id) {
                    continue;
                }
                let Some(metres) =
                    exactly_to(target, SpatialExtent::decode(value.as_slice())?.bounds)
                else {
                    return Ok(None);
                };
                ranked.insert(id.clone(), metres);
                if best.len() < wanted {
                    best.push(Ranked {
                        metres,
                        id: id.clone(),
                    });
                } else if best.peek().is_some_and(|worst| metres < worst.metres) {
                    best.pop();
                    best.push(Ranked { metres, id });
                }
            }
            if whole {
                continue;
            }
            for child in next.cell.children().into_iter().flatten() {
                if let Some(extent) = child.extent() {
                    frontier.push(Frontier {
                        bound: tessari_geo::no_closer_than(target, extent),
                        cell: child,
                    });
                }
            }
        }

        if best.len() < wanted {
            return Ok(None);
        }
        // Sorted by distance and then by identity, which is the order the value
        // system breaks a tie in, so the answer matches a scan's down to the
        // records that are exactly the same distance away.
        let mut placed: Vec<(f64, RecordId)> = ranked
            .into_iter()
            .map(|(id, metres)| (metres, id))
            .collect();
        placed.sort_by(|(one, left), (other, right)| {
            one.total_cmp(other).then_with(|| left.cmp(right))
        });
        // The tie group at the bound travels with the answer: two records the
        // same distance away are both in it or neither is.
        let edge = placed
            .get(wanted.saturating_sub(1))
            .map_or(f64::INFINITY, |(metres, _)| *metres);
        placed.retain(|(metres, _)| *metres <= edge);

        let addresses: Vec<RecordAddress> = placed
            .iter()
            .map(|(_, id)| {
                RecordAddress::new(index.namespace, index.database, index.table, id.clone())
            })
            .collect();
        let rows = addresses
            .iter()
            .zip(self.get_each(&addresses)?)
            .filter_map(|(address, payload)| Some((address.id.clone(), payload?)))
            .collect();
        Ok(Some(Nearby {
            rows,
            entries,
            expanded,
        }))
    }
}
