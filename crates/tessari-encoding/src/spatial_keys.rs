//! Spatial index entries.
//!
//! ```text
//! key    <0x16> <ns:u32> <db:u32> <tb:u32> <ix:u32> <first:u64> <level:u8> <record-id>
//! value  <west:i64> <south:i64> <east:i64> <north:i64>
//! ```
//!
//! One entry per cell of the record's covering, so a record whose geometry spans
//! several cells has several — which is what a covering *is*, and the reason this
//! is not a secondary index with a cell for a value.
//!
//! # Why the key carries the start of the cell's range and not its index
//!
//! A cell's index numbers it among the cells of its own level, so it is small at
//! a coarse level and large at a fine one. Sorting entries by index would
//! therefore interleave *levels*, and a scan over a range of cells would return
//! an arbitrary set. The start of the cell's range does not have that problem:
//! every descendant of a cell has its start inside that cell's range, so
//!
//! - **a cell's descendants are one contiguous scan**, and
//! - **a cell's ancestors are a bounded, computable set** — at most `level` of
//!   them, each recovered by truncating the start.
//!
//! Both halves are load-bearing and the second is the one that is easy to miss:
//! a record larger than the query box sits at a *coarser* cell, whose start lies
//! below the query cell's range, so a scan alone never finds it. A reader that
//! only scanned would silently return fewer rows than exist — the failure
//! direction a spatial filter must not have. The level byte follows the start so
//! that the cells covering one square group together whatever their level.
//!
//! # Why the box is in the value
//!
//! A cell is coarser than the box that produced it, so a cell match is a
//! candidate and never a result. Carrying the record's own bounding box in the
//! entry lets the filter step reject a candidate without decoding the record,
//! and lets a question about extent alone be answered without touching the
//! geometry at all.
//!
//! This is also where the write-path invariant lives. The box is computed in the
//! batch that carries the record's mutation and by nothing else — no background
//! job, no repair pass, no reader. A box maintained anywhere else can lag its
//! geometry, and a stale box excludes rows that should have matched with nothing
//! raised anywhere.

use tessari_geo::{Bounds, Cell};
use tessari_kv::{Key, Value};
use tessari_types::RecordId;

use crate::error::{Error, Result};
use crate::index_keys::IndexAddress;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};

/// How many bytes of a spatial key name the cell: the start of its range,
/// then its level.
const CELL_LEN: usize = 9;

/// One cell of one record's covering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpatialIndexKey {
    /// Which index this entry belongs to.
    pub address: IndexAddress,
    /// The cell of the record's covering this entry stands for.
    pub cell: Cell,
    /// The record the entry points at.
    pub id: RecordId,
}

impl SpatialIndexKey {
    /// Name one entry.
    #[must_use]
    pub const fn new(address: IndexAddress, cell: Cell, id: RecordId) -> Self {
        Self { address, cell, id }
    }

    /// The prefix every entry in one cell shares — a fixed width, so "every
    /// entry in this cell" is a prefix scan the way "every entry of this index"
    /// already is.
    fn cell_prefix(address: &IndexAddress, cell: Cell) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::SpatialIndex);
        let mut writer = KeyWriter::with_capacity(CELL_LEN);
        writer.put_u64(cell.range().0).put_u8(level_byte(cell));
        bytes.extend_from_slice(&writer.finish());
        bytes
    }
}

/// A cell's level as one byte.
///
/// [`tessari_geo::ORDER`] is 32, so every level fits; the conversion is written
/// out rather than cast because a widened `ORDER` should fail here loudly rather
/// than wrap a level 256 into level 0.
fn level_byte(cell: Cell) -> u8 {
    u8::try_from(cell.level()).unwrap_or(u8::MAX)
}

impl StoreKey for SpatialIndexKey {
    type Value = SpatialExtent;

    const KIND: KeyKind = KeyKind::SpatialIndex;

    fn encode(&self) -> Key {
        let mut bytes = Self::cell_prefix(&self.address, self.cell);
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let first = reader.take_u64()?;
        let level = reader.take_u8()?;
        let level = u32::from(level);
        let cell = Cell::starting_at(level, first).ok_or(Error::NoSuchCell { level, first })?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self { address, cell, id })
    }
}

/// The record's bounding box, as the filter step reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialExtent {
    /// The box around the record's whole geometry.
    pub bounds: Bounds,
}

impl SpatialExtent {
    /// State the box.
    #[must_use]
    pub const fn new(bounds: Bounds) -> Self {
        Self { bounds }
    }
}

impl StoreValue for SpatialExtent {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_i64(self.bounds.west())
            .put_i64(self.bounds.south())
            .put_i64(self.bounds.east())
            .put_i64(self.bounds.north());
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SpatialIndex, payload);
        let west = reader.take_i64()?;
        let south = reader.take_i64()?;
        let east = reader.take_i64()?;
        let north = reader.take_i64()?;
        reader.finish()?;
        let bounds = Bounds::of_corners(west, south, east, north).ok_or(Error::NotABox {
            west,
            south,
            east,
            north,
        })?;
        Ok(Self { bounds })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tessari_geo::{Cell, ORDER, Snapped};
    use tessari_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

    use super::{Bounds, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue};
    use crate::error::Error;
    use crate::index_keys::{INDEX_PREFIX_LEN, IndexAddress};
    use crate::kind::KeyKind;

    fn address() -> IndexAddress {
        IndexAddress::new(
            NamespaceId::new(3),
            DatabaseId::new(4),
            TableId::new(5),
            IndexId::new(6),
        )
    }

    fn at(longitude: i64, latitude: i64) -> Snapped {
        Snapped::from_units(longitude, latitude).unwrap()
    }

    /// A deterministic sequence, so a failure is reproducible from its own output.
    struct Spread(u64);

    impl Spread {
        fn next(&mut self) -> u64 {
            let mut state = self.0;
            state ^= state.wrapping_shl(13);
            state ^= state.wrapping_shr(7);
            state ^= state.wrapping_shl(17);
            self.0 = state;
            state
        }
    }

    /// The cell of `position` at `level`.
    fn cell_at(position: Snapped, level: u32) -> Cell {
        let finest = Cell::containing(position);
        Cell::starting_at(level, coarsen(finest.range().0, level)).unwrap()
    }

    /// The start of the range of the level-`level` cell holding `first`.
    fn coarsen(first: u64, level: u32) -> u64 {
        let width = 2_u32.saturating_mul(ORDER.saturating_sub(level));
        if width >= u64::BITS {
            return 0;
        }
        first
            .checked_shr(width)
            .and_then(|index| index.checked_shl(width))
            .unwrap_or(0)
    }

    #[test]
    fn an_entry_survives_the_round_trip_at_every_level() {
        let mut spread = Spread(0x5a71_a100);
        for up in 0..=ORDER {
            let position = at(
                i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
                i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
            );
            let key = SpatialIndexKey::new(
                address(),
                cell_at(position, ORDER.saturating_sub(up)),
                RecordId::from("r"),
            );
            let encoded = key.encode();
            assert_eq!(
                SpatialIndexKey::decode(encoded.as_slice()).unwrap(),
                key,
                "an entry {up} level(s) above the finest should read back unchanged"
            );
        }
    }

    #[test]
    fn an_entry_at_any_level_sorts_inside_the_run_of_every_cell_above_it() {
        // The property the whole layout exists for, and the one a query side
        // will rest on: an entry must be findable by scanning the byte range of
        // any coarser cell containing it, **whatever level the entry sits at**.
        //
        // The levels are the load-bearing part of the generator. A version of
        // this test that only placed entries at the finest level passed with the
        // level byte moved in front of the range start — every key then carried
        // the same leading byte, so its position could not matter and the
        // assertion never reached the case it exists for. A real covering mixes
        // levels, and mixed levels are what the ordering has to survive.
        //
        // The bounds are built from `to_be_bytes` rather than through the key
        // writer, so the comparison is a second statement of the layout rather
        // than the encoder agreeing with itself. They deliberately stop after
        // the range start: the level byte and the record id both follow, and a
        // bound that pinned a level would exclude exactly the coarser entries
        // this test is about.
        let mut spread = Spread(0x0d0e_5c3d);
        for _ in 0..32 {
            let position = at(
                i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
                i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
            );
            for down in 0..=ORDER {
                let entry =
                    SpatialIndexKey::new(address(), cell_at(position, down), RecordId::from("r"))
                        .encode();
                // Coarser is a *lower* level, so an entry at `down` has its
                // ancestors below it and not above it in the numbering.
                for up in 0..down {
                    let (first, last) = cell_at(position, up).range();
                    let mut low = address().prefix(KeyKind::SpatialIndex);
                    low.extend_from_slice(&first.to_be_bytes());
                    let mut high = address().prefix(KeyKind::SpatialIndex);
                    high.extend_from_slice(&last.to_be_bytes());
                    high.extend_from_slice(&[u8::MAX; 9]);
                    assert!(
                        low.as_slice() <= entry.as_slice() && entry.as_slice() <= high.as_slice(),
                        "a level-{down} entry should sort inside the run of the level-{up} cell above it"
                    );
                }
            }
        }
    }

    #[test]
    fn a_level_and_a_start_that_name_no_cell_are_refused() {
        // What a corrupt key looks like: the level says the cell is coarse, the
        // start says it began somewhere no cell of that level does. Accepting it
        // would hand a reader a square nothing ever wrote.
        let key = SpatialIndexKey::new(
            address(),
            Cell::starting_at(ORDER, 7).unwrap(),
            RecordId::from("r"),
        );
        let mut bytes = key.encode().as_slice().to_vec();
        // The level byte, named rather than counted from the end: the record id
        // is variable width, so an offset measured backwards would land
        // somewhere else the moment a test used a different id.
        bytes[INDEX_PREFIX_LEN.saturating_add(8)] = 1;
        assert!(matches!(
            SpatialIndexKey::decode(&bytes),
            Err(Error::NoSuchCell { .. })
        ));
    }

    #[test]
    fn a_box_survives_the_round_trip_and_a_backwards_one_is_refused() {
        let held = SpatialExtent::new(
            Bounds::of_position(at(-1_000, -2_000)).widened_to(at(3_000, 4_000)),
        );
        let encoded = held.encode();
        assert_eq!(SpatialExtent::decode(encoded.as_slice()).unwrap(), held);

        // West east of east: not a box any geometry produces, and a rectangle
        // that would answer `meets` for a strip while holding no position.
        let mut bytes = encoded.as_slice().to_vec();
        let body = bytes.len().saturating_sub(32);
        bytes[body..body.saturating_add(8)].copy_from_slice(&[0xFF; 8]);
        assert!(matches!(
            SpatialExtent::decode(&bytes),
            Err(Error::NotABox { .. })
        ));
    }
}
