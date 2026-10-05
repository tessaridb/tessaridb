//! Keys for a vector index's graph and for the recall and refinement figures measured over an index.

use super::IndexAddress;
use super::quantized::{QuantizedVector, StoredVector};
use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};
use tessari_kv::{Key, Value};
use tessari_types::RecordId;

/// The recall one vector index was last measured at.
///
/// A vector index answers **approximately**, so the only number that says
/// whether its answers are worth having is the fraction of the true nearest it
/// actually returns. That is a property of a measurement and not of a
/// declaration — a figure derived from the build parameters would be a number
/// nobody checked wearing the name of one somebody did.
///
/// Beside the index rather than on its definition, for the reason
/// [`SearchStatisticsKey`] is: this is a measurement derived from the log, and
/// the definition is what the language wrote. The key is **derived from the
/// [`IndexAddress`]** rather than stored beside it, so it cannot come to name
/// the wrong index, and it is cleared as part of the index's keyspace when the
/// entries are — which is what stops a rebuild leaving a figure describing a
/// graph that no longer exists.
///
/// The key is exactly an index prefix with no suffix, so one index has exactly
/// one of these and finding it is a point read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorRecallKey {
    /// Which index this measurement describes.
    pub address: IndexAddress,
}

impl VectorRecallKey {
    /// Name the measurement of one index.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for VectorRecallKey {
    type Value = VectorRecall;

    const KIND: KeyKind = KeyKind::VectorRecall;

    fn encode(&self) -> Key {
        Key::from(self.address.prefix(Self::KIND))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { address })
    }
}

/// A measured recall, and everything needed to read it.
///
/// # Why a bare percentage is not stored
///
/// Recall decays as records are added after the measurement — the graph keeps
/// answering, and answers less of the truth — so a lone figure describes a store
/// that may no longer exist, and it goes stale in silence. Every field here
/// exists so a reader can tell whether the number still means anything:
///
/// - `at` — recall@10 and recall@1 are different numbers.
/// - `sample` — a figure from four queries is not a figure from four hundred.
/// - `records` — how large the store was when it was measured, so growth since
///   is visible rather than hidden.
/// - `neighbours` and `exploration` — the engine constants in force; a recall
///   measured at one budget does not describe another.
///
/// Absence of this value means **never measured**, which is a different
/// statement from a measured zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorRecall {
    /// The fraction of the true nearest the walk returned, as a percentage.
    pub recall: u32,
    /// How many neighbours each query asked for.
    pub at: u32,
    /// How many queries the figure is an average over.
    pub sample: u32,
    /// How many records the index held when it was measured.
    pub records: u64,
    /// The neighbour count each node was built with.
    pub neighbours: u32,
    /// The exploration budget the measuring walks spent.
    pub exploration: u32,
}

impl StoreValue for VectorRecall {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u32(self.recall)
            .put_u32(self.at)
            .put_u32(self.sample)
            .put_u64(self.records)
            .put_u32(self.neighbours)
            .put_u32(self.exploration);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::VectorRecall, payload);
        let recall = reader.take_u32()?;
        let at = reader.take_u32()?;
        let sample = reader.take_u32()?;
        let records = reader.take_u64()?;
        let neighbours = reader.take_u32()?;
        let exploration = reader.take_u32()?;
        reader.finish()?;
        Ok(Self {
            recall,
            at,
            sample,
            records,
            neighbours,
            exploration,
        })
    }
}

/// What refining one spatial index's candidates last cost.
///
/// A spatial index answers by **bounding box**, and a box is not a geometry — so
/// every read is filter-and-refine, and the number that says whether the filter
/// is earning its keep is how many records it offers against how many survive.
/// A ratio near one means the stored boxes approximate their geometries well; a
/// large one means the index is doing work the exact predicate throws away.
/// Without it a query budget is tuned by intuition, and a structurally bad row —
/// a river, a road, a border, whose box is many times its own area — is
/// invisible.
///
/// Beside the index rather than on its definition, for the reason
/// [`VectorRecallKey`] is: this is a measurement derived from the log, and the
/// definition is what the language wrote. The key is **derived from the
/// [`IndexAddress`]**, so it cannot come to name the wrong index, and it is
/// cleared as part of the index's keyspace when the entries are — which is what
/// stops a rebuild leaving a figure describing a covering that no longer exists.
///
/// The key is exactly an index prefix with no suffix, so one index has exactly
/// one of these and finding it is a point read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialRefinementKey {
    /// Which index this measurement describes.
    pub address: IndexAddress,
}

impl SpatialRefinementKey {
    /// Name the measurement of one index.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for SpatialRefinementKey {
    type Value = SpatialRefinement;

    const KIND: KeyKind = KeyKind::SpatialRefinement;

    fn encode(&self) -> Key {
        Key::from(self.address.prefix(Self::KIND))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { address })
    }
}

/// The counts a covering is judged by, and everything needed to read them.
///
/// # Why counts rather than a ratio
///
/// There are **two** ratios here and they name two different repairs, so a
/// single stored number would answer neither question:
///
/// - `reached` over `admitted` is how loose the stored boxes are. The cells
///   offer records the box test then throws away.
/// - `entries` over `reached` is how fragmented the covering is. One record with
///   an awkward shape occupies many cells, and every one of them is an entry the
///   traversal reads to arrive at the same record — which is exactly the river,
///   the road and the border the measurement exists to make visible.
///
/// Storing the counts leaves both derivable and neither asserted.
///
/// # Why it cannot go stale in silence
///
/// The counts describe the store as it was when the index was built. `sample`
/// says how many queries stand behind them — a figure from four is not a figure
/// from four hundred — and `records` says how large the index was, so growth
/// since is visible rather than hidden.
///
/// # The query record is not counted
///
/// Each measuring query is a record's own box, and a record always reaches
/// itself and always survives its own box test. Counting it would add one to
/// both sides of every ratio and pull each one toward one, which is to say
/// toward "healthy" — so the record being asked about is excluded from both.
/// A store whose records never reach one another therefore measures nothing at
/// all rather than measuring a perfect score.
///
/// Absence of this value means **never measured**, which is a different
/// statement from a measured zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialRefinement {
    /// Index entries the measuring reads walked.
    pub entries: u64,
    /// Distinct records those entries named.
    pub reached: u64,
    /// How many of those the box test admitted.
    pub admitted: u64,
    /// How many queries the counts are a total over.
    pub sample: u32,
    /// How many records the index held when it was measured.
    pub records: u64,
}

impl SpatialRefinement {
    /// Records offered per record kept, as a percentage.
    ///
    /// `None` when nothing was admitted — not because there was no measurement,
    /// but because the ratio is unbounded there. A covering that offered records
    /// and kept none is the worst refinement there is, and the counts beside this
    /// say so plainly; collapsing it to a number would either invent a ceiling or
    /// print `0`, which reads as a perfect filter.
    ///
    /// `checked_div` rather than a guard and a `saturating_div`: the absence and
    /// the division are the same fact, so writing them as two would let a later
    /// edit separate them. `saturating_div` also does not saturate a zero
    /// divisor — it panics — which is a poor thing to reach for in a database.
    #[must_use]
    pub const fn refinement(self) -> Option<u64> {
        self.reached.saturating_mul(100).checked_div(self.admitted)
    }

    /// Entries read per record reached, as a percentage.
    ///
    /// `None` for the same reason [`Self::refinement`] returns one.
    #[must_use]
    pub const fn fragmentation(self) -> Option<u64> {
        self.entries.saturating_mul(100).checked_div(self.reached)
    }
}

impl StoreValue for SpatialRefinement {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u64(self.entries)
            .put_u64(self.reached)
            .put_u64(self.admitted)
            .put_u32(self.sample)
            .put_u64(self.records);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SpatialRefinement, payload);
        let entries = reader.take_u64()?;
        let reached = reader.take_u64()?;
        let admitted = reader.take_u64()?;
        let sample = reader.take_u32()?;
        let records = reader.take_u64()?;
        reader.finish()?;
        Ok(Self {
            entries,
            reached,
            admitted,
            sample,
            records,
        })
    }
}

/// One record's place in a vector index's graph.
///
/// ```text
/// key    <0x13> <ns:u32> <db:u32> <tb:u32> <ix:u32> <level:u8> <record-id>
/// value  <dimensions:u32> <component:f64 × dimensions> <neighbours:u32> <record-id × neighbours>
/// ```
///
/// # The level byte is reserved and always zero
///
/// A hierarchical graph assigns each node a level, and the layers improve
/// routing at large collection sizes. This index has one layer, because a level
/// drawn from a generator is exactly what a store whose index entries are
/// **derived rather than logged** cannot have: two replicas would build
/// different graphs from one log and disagree, silently, about which ten records
/// are nearest.
///
/// The byte is in the key anyway. Reserving room costs nothing today and cannot
/// be done retroactively — the same argument the key-kind table itself makes —
/// and it sorts before the record id so a future level's nodes group together.
///
/// # The vector is in the node
///
/// A walk visits many nodes and answers with few, so carrying the vector here
/// means the search touches index keys and decodes no records until the answer
/// is chosen. Measured on this store, decoding the records is about a seventh of
/// a linear nearest-neighbour read; here it is avoided as a side effect rather
/// than pursued as a feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorNodeKey {
    /// Which index this node belongs to.
    pub address: IndexAddress,
    /// Which layer. Always zero until the graph has more than one.
    pub level: u8,
    /// The record the node stands for.
    pub id: RecordId,
}

impl VectorNodeKey {
    /// Name one node.
    #[must_use]
    pub const fn new(address: IndexAddress, level: u8, id: RecordId) -> Self {
        Self { address, level, id }
    }

    /// The prefix every node of one level shares.
    #[must_use]
    pub fn level_prefix(address: &IndexAddress, level: u8) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::VectorNode);
        bytes.push(level);
        bytes
    }
}

impl StoreKey for VectorNodeKey {
    type Value = VectorNode;

    const KIND: KeyKind = KeyKind::VectorNode;

    fn encode(&self) -> Key {
        let mut bytes = Self::level_prefix(&self.address, self.level);
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let level = reader.take_u8()?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self { address, level, id })
    }
}

/// What a node holds: the record's vector, and who it points at.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorNode {
    /// The record's vector — every component, or one byte per component in a
    /// `QUANTIZED` index.
    pub vector: StoredVector,
    /// The records this node links to, in the order the graph chose.
    pub neighbours: Vec<RecordId>,
}

/// The header flag of a node whose vector is held as codes.
///
/// A full-precision node is written with no flag, byte for byte as every node
/// was before quantization existed, so an index written then reads unchanged.
const QUANTIZED: u8 = 1;

impl VectorNode {
    /// Build a node.
    #[must_use]
    pub const fn new(vector: StoredVector, neighbours: Vec<RecordId>) -> Self {
        Self { vector, neighbours }
    }
}

impl StoreValue for VectorNode {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        let flags = match &self.vector {
            StoredVector::Full(vector) => {
                writer.put_u32(u32::try_from(vector.len()).unwrap_or(u32::MAX));
                for component in vector {
                    // The bit pattern, not a decimal projection: this is storage
                    // for arithmetic rather than an index key, so nothing here
                    // has to sort.
                    writer.put_u64(component.to_bits());
                }
                0
            }
            StoredVector::Quantized(coded) => {
                writer.put_u32(u32::try_from(coded.codes.len()).unwrap_or(u32::MAX));
                writer.put_u64(coded.low.to_bits());
                writer.put_u64(coded.step.to_bits());
                for code in &coded.codes {
                    writer.put_u8(*code);
                }
                QUANTIZED
            }
        };
        writer.put_u32(u32::try_from(self.neighbours.len()).unwrap_or(u32::MAX));
        for neighbour in &self.neighbours {
            record_id::put(&mut writer, neighbour);
        }
        let body = writer.finish();
        let mut buffer = with_header(flags, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (flags, payload) = split_header(bytes, QUANTIZED)?;
        let mut reader = KeyReader::new(KeyKind::VectorNode, payload);
        let dimensions = reader.take_u32()?;
        // Reserved no further than the bytes could hold: the width is the entry's
        // claim, and a damaged entry claiming 4 billion must cost what it holds.
        let width = usize::try_from(dimensions).unwrap_or(0).min(payload.len());
        let vector = if flags & QUANTIZED == 0 {
            let mut vector = Vec::with_capacity(width);
            for _ in 0..dimensions {
                vector.push(f64::from_bits(reader.take_u64()?));
            }
            StoredVector::Full(vector)
        } else {
            let low = f64::from_bits(reader.take_u64()?);
            let step = f64::from_bits(reader.take_u64()?);
            let mut codes = Vec::with_capacity(width);
            for _ in 0..dimensions {
                codes.push(reader.take_u8()?);
            }
            StoredVector::Quantized(QuantizedVector { low, step, codes })
        };
        let count = reader.take_u32()?;
        let mut neighbours = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        for _ in 0..count {
            neighbours.push(record_id::take(&mut reader)?);
        }
        reader.finish()?;
        Ok(Self { vector, neighbours })
    }
}
