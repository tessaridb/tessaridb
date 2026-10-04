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
use tessari_kv::{Key, KeyRange, Value};
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
    ///
    /// This is the **ancestor** half of a lookup. A record larger than the query
    /// box sits at a coarser cell, and a coarser cell is named exactly, one
    /// prefix per level, rather than searched for.
    #[must_use]
    pub fn cell_prefix(address: &IndexAddress, cell: Cell) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::SpatialIndex);
        let mut writer = KeyWriter::with_capacity(CELL_LEN);
        writer.put_u64(cell.range().0).put_u8(level_byte(cell));
        bytes.extend_from_slice(&writer.finish());
        bytes
    }

    /// Every entry at or below one cell, as one span of the key space.
    ///
    /// This is the **descendant** half of a lookup, and it is one scan because
    /// every cell under `cell` has its range start inside `cell`'s range — which
    /// is the property the key stores a range start for.
    ///
    /// The span begins at the cell's own start with nothing after it, so it
    /// takes in the cell itself at every level that shares that start, and ends
    /// just past the last number the cell covers. A cell whose range reaches the
    /// end of the numbering has no such successor; that is the root, and the
    /// answer for the root is the whole index.
    #[must_use]
    pub fn descendants(address: &IndexAddress, cell: Cell) -> KeyRange {
        let prefix = address.prefix(KeyKind::SpatialIndex);
        let (first, last) = cell.range();
        let Some(past) = last.checked_add(1) else {
            return KeyRange::prefix(&prefix);
        };
        // Written through the key writer rather than as bytes, because a bound
        // that does not agree with the encoder is a scan over the wrong span,
        // and a scan over the wrong span answers with fewer rows and no error.
        KeyRange::between(
            Key::from(at_number(&prefix, first)),
            Key::from(at_number(&prefix, past)),
        )
    }
}

/// An index prefix followed by one finest-level number, and nothing else.
fn at_number(prefix: &[u8], number: u64) -> Vec<u8> {
    let mut bytes = prefix.to_vec();
    let mut writer = KeyWriter::with_capacity(8);
    writer.put_u64(number);
    bytes.extend_from_slice(&writer.finish());
    bytes
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
mod tests;
