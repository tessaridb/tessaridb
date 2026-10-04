//! A spatial index walked nearest-first, one record at a time (G058 C1).
//!
//! The walk yields records in the order of a **floor** — the distance from the
//! target to the record's stored box, which nothing in the record can be nearer
//! than — and leaves the measuring to the caller. That split is what lets one
//! walk serve a read over areas and a read under a condition: the caller decodes
//! each record, tests the whole condition, measures the exact distance by the
//! statement's own expression, and stops once it holds as many as it wants and
//! the next floor is beyond the worst of them.
//!
//! # Why the order is safe
//!
//! Cells and records wait in one queue keyed by their floors. A record's nearest
//! point lies in some cell of its covering, whose floor is no greater than that
//! point's distance, and so are the floors of every cell above it; that cell is
//! opened — and the record put in the queue on a floor no greater than its true
//! distance — before the queue passes that distance. So a record whose exact
//! distance is `d` always comes out before any floor beyond `d`.
//!
//! Each record comes out once, however many cells of its covering are opened.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};

use tessari_constants::SPATIAL_WALK_SUBTREE_ENTRIES;
use tessari_encoding::{
    IndexAddress, KeyKind, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue,
};
use tessari_kv::KeyRange;
use tessari_types::RecordId;

use super::Transaction;
use crate::catalog::IndexDefinition;
use crate::error::Result;

/// A nearest-first walk over one spatial index, from one position.
#[derive(Debug)]
pub struct PlacesNearest {
    address: IndexAddress,
    target: tessari_geo::Snapped,
    waiting: BinaryHeap<Waiting>,
    queued: BTreeSet<RecordId>,
    /// How many index entries the walk has read.
    pub entries: usize,
    /// How many cells it has opened.
    pub expanded: usize,
}

#[derive(Debug)]
enum Item {
    Cell(tessari_geo::Cell),
    Record(RecordId),
}

/// One thing waiting, cheapest floor first.
#[derive(Debug)]
struct Waiting {
    floor: f64,
    item: Item,
}

impl Ord for Waiting {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed for the max-heap; at one floor a record before a cell, so a
        // record is handed out before anything at its distance is opened.
        other.floor.total_cmp(&self.floor).then_with(|| {
            matches!(self.item, Item::Record(_)).cmp(&matches!(other.item, Item::Record(_)))
        })
    }
}

impl PartialOrd for Waiting {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Waiting {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Waiting {}

impl Transaction<'_> {
    /// Begin a nearest-first walk of `index` from `target`.
    #[must_use]
    pub fn places_nearest(
        &self,
        index: &IndexDefinition,
        target: tessari_geo::Snapped,
    ) -> PlacesNearest {
        let mut waiting = BinaryHeap::new();
        waiting.push(Waiting {
            floor: 0.0,
            item: Item::Cell(tessari_geo::Cell::root()),
        });
        PlacesNearest {
            address: IndexAddress::new(index.namespace, index.database, index.table, index.id),
            target,
            waiting,
            queued: BTreeSet::new(),
            entries: 0,
            expanded: 0,
        }
    }
}

impl PlacesNearest {
    /// The next record, with a floor no greater than its true distance — or
    /// `None` when nothing is left at or below `beyond`.
    ///
    /// Floors come out in order, so `beyond` lets a caller holding its answer
    /// stop the walk without opening cells further away than the worst record
    /// it keeps.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or an entry cannot be decoded.
    pub fn next(
        &mut self,
        transaction: &Transaction<'_>,
        beyond: f64,
    ) -> Result<Option<(f64, RecordId)>> {
        let keyspace = KeyKind::SpatialIndex.keyspace();
        while let Some(Waiting { floor, item }) = self.waiting.pop() {
            if floor > beyond {
                self.waiting.push(Waiting { floor, item });
                return Ok(None);
            }
            let cell = match item {
                Item::Record(id) => return Ok(Some((floor, id))),
                Item::Cell(cell) => cell,
            };
            self.expanded = self.expanded.saturating_add(1);
            let subtree = transaction.scan_once(
                keyspace,
                SpatialIndexKey::descendants(&self.address, cell),
                SPATIAL_WALK_SUBTREE_ENTRIES.saturating_add(1),
            )?;
            let whole = subtree.len() <= SPATIAL_WALK_SUBTREE_ENTRIES;
            let read = if whole {
                subtree
            } else {
                // Too big to take at once: what is stored at this exact cell
                // now, and the four children on floors of their own.
                transaction.scan_once(
                    keyspace,
                    KeyRange::prefix(&SpatialIndexKey::cell_prefix(&self.address, cell)),
                    usize::MAX,
                )?
            };
            for (key, value) in &read {
                self.entries = self.entries.saturating_add(1);
                let id = SpatialIndexKey::decode(key.as_slice())?.id;
                if self.queued.contains(&id) {
                    continue;
                }
                let bounds = SpatialExtent::decode(value.as_slice())?.bounds;
                self.queued.insert(id.clone());
                self.waiting.push(Waiting {
                    floor: tessari_geo::no_closer_than(self.target, bounds),
                    item: Item::Record(id),
                });
            }
            if whole {
                continue;
            }
            for child in cell.children().into_iter().flatten() {
                if let Some(extent) = child.extent() {
                    self.waiting.push(Waiting {
                        floor: tessari_geo::no_closer_than(self.target, extent),
                        item: Item::Cell(child),
                    });
                }
            }
        }
        Ok(None)
    }
}
