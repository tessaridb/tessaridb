//! Keys for a partitioned vector index: its trained state and its codes.
//!
//! A partitioned (IVF, product-quantized) vector index keeps no graph. It
//! keeps what a probe needs: the centroids that define its partitions and the
//! codebooks that compress its vectors (one trained key per index), and, per
//! record, one byte of code per subspace under a key that names the record's
//! partition (ADR-0141).
//!
//! ```text
//! trained  <0x49> <ns:u32> <db:u32> <tb:u32> <ix:u32>
//!          <partitions:u32> <dimensions:u32> <subspaces:u32> <codebook-size:u32>
//!          <centroid:f64 × partitions×dimensions>
//!          <codebook:f64 × subspaces×codebook-size×ceil(dimensions/subspaces)>
//!
//! codes    <0x4a> <ns:u32> <db:u32> <tb:u32> <ix:u32> <partition:u32> <record-id>
//!          <code:u8 × subspaces>
//! ```
//!
//! # The partition is in the key
//!
//! Probing reads whole partitions, so a partition has to be one range: the
//! key orders partition before record id, and a probe is a single prefix
//! scan. A filtered read pays a distance only for a record the predicate
//! already admitted (ADR-0141 D3) — the codes it scans past are bytes, not
//! arithmetic.
//!
//! # The trained state is derived, and travels like the entries
//!
//! Both values are built by applying a log record, from the rows and the
//! index identity alone — a seeded k-means with fixed iterations over
//! id-sorted samples — so every replica derives byte-identical state and the
//! producing node has nothing to send but the log (ADR-0141 D1). The trained
//! key is cleared with the entries: a rebuilt index re-derives everything,
//! and a half-cleared state is not a state this store can reason about.

use super::IndexAddress;
use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};
use tessari_kv::{Key, Value};
use tessari_types::RecordId;

/// The trained state of one partitioned vector index: its centroids and its
/// product-quantizer codebooks.
///
/// Exactly one per index — the key is an index prefix with no suffix, so
/// finding it is a point read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorTrainedKey {
    /// Which index this state belongs to.
    pub address: IndexAddress,
}

impl VectorTrainedKey {
    /// Name one index's trained state.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for VectorTrainedKey {
    type Value = VectorTrained;

    const KIND: KeyKind = KeyKind::VectorTrained;

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

/// What training derived, and everything a probe needs to use it.
///
/// `centroids` holds `partitions × dimensions` components, row by row.
/// `codebooks` holds, per subspace, 256 centroids of `subspace_width`
/// components each, where `subspace_width = dimensions.div_ceil(subspaces)`.
/// The last subspace may be narrower; its missing components read as the
/// stored value never reached.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorTrained {
    /// How many partitions the corpus was split into.
    pub partitions: u32,
    /// The width of every vector the index was trained over.
    pub dimensions: u32,
    /// How many subspaces product quantization split the width into.
    pub subspaces: u32,
    /// How many centroids one subspace's codebook holds — up to 256, fewer
    /// when the corpus trained on had fewer samples than that.
    pub codebook_size: u32,
    /// The partition centroids, row by row: `partitions × dimensions`.
    pub centroids: Vec<f64>,
    /// The product-quantizer codebooks, subspace by subspace: `subspaces ×
    /// codebook_size × subspace_width`.
    pub codebooks: Vec<f64>,
}

impl VectorTrained {
    /// The width one subspace codes; the last subspace may be narrower.
    #[must_use]
    pub fn subspace_width(&self) -> usize {
        usize::try_from(self.dimensions.div_ceil(self.subspaces.max(1))).unwrap_or(0)
    }

    /// The centroid of one partition.
    #[must_use]
    pub fn centroid(&self, partition: u32) -> &[f64] {
        let width = usize::try_from(self.dimensions).unwrap_or(0);
        let at = usize::try_from(partition)
            .unwrap_or(0)
            .saturating_mul(width);
        self.centroids
            .get(at..at.saturating_add(width))
            .unwrap_or(&[])
    }

    /// This subspace's codebook centroids.
    #[must_use]
    pub fn codebook(&self, subspace: usize) -> &[f64] {
        let width = self.subspace_width();
        let size = usize::try_from(self.codebook_size).unwrap_or(0);
        let at = subspace.saturating_mul(size).saturating_mul(width);
        self.codebooks
            .get(at..at.saturating_add(size.saturating_mul(width)))
            .unwrap_or(&[])
    }
}

/// `u32::div_ceil` guarded against a zero divisor, used where a width is
/// derived from untrusted stored values.
#[must_use]
fn usize_div_ceil(held: u32, by: u32) -> usize {
    usize::try_from(held.div_ceil(by.max(1))).unwrap_or(0)
}

impl StoreValue for VectorTrained {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u32(self.partitions)
            .put_u32(self.dimensions)
            .put_u32(self.subspaces)
            .put_u32(self.codebook_size);
        for component in &self.centroids {
            writer.put_u64(component.to_bits());
        }
        for component in &self.codebooks {
            writer.put_u64(component.to_bits());
        }
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::VectorTrained, payload);
        let partitions = reader.take_u32()?;
        let dimensions = reader.take_u32()?;
        let subspaces = reader.take_u32()?;
        let codebook_size = reader.take_u32()?;
        // Reserved no further than the bytes could hold: a damaged value must
        // cost what it holds, not what its header claims.
        let remaining = payload.len().saturating_sub(16);
        let stated = usize::try_from(partitions)
            .unwrap_or(0)
            .saturating_mul(usize::try_from(dimensions).unwrap_or(0))
            .saturating_add(
                usize::try_from(subspaces)
                    .unwrap_or(0)
                    .saturating_mul(usize::try_from(codebook_size).unwrap_or(0))
                    .saturating_mul(usize_div_ceil(dimensions, subspaces)),
            )
            .saturating_mul(8);
        let total = stated.min(remaining).checked_div(8).unwrap_or(0);
        let mut components = Vec::with_capacity(total.min(1 << 20));
        for _ in 0..total {
            components.push(f64::from_bits(reader.take_u64()?));
        }
        let (centroids, codebooks) = components.split_at(
            usize::try_from(partitions)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(dimensions).unwrap_or(0))
                .min(components.len()),
        );
        reader.finish()?;
        Ok(Self {
            partitions,
            dimensions,
            subspaces,
            codebook_size,
            centroids: centroids.to_vec(),
            codebooks: codebooks.to_vec(),
        })
    }
}

/// One record's codes in one partitioned vector index.
///
/// The key names the record's partition before the record, so one partition
/// is one prefix range and a probe never leaves its keyspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorCodeKey {
    /// Which index this code belongs to.
    pub address: IndexAddress,
    /// Which of the trained partitions holds the record.
    pub partition: u32,
    /// The record the codes stand for.
    pub id: RecordId,
}

impl VectorCodeKey {
    /// Name one record's codes.
    #[must_use]
    pub const fn new(address: IndexAddress, partition: u32, id: RecordId) -> Self {
        Self {
            address,
            partition,
            id,
        }
    }

    /// The prefix every code of one partition shares.
    #[must_use]
    pub fn partition_prefix(address: &IndexAddress, partition: u32) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::VectorCodes);
        let mut writer = KeyWriter::new();
        writer.put_u32(partition);
        bytes.extend_from_slice(&writer.finish());
        bytes
    }
}

impl StoreKey for VectorCodeKey {
    type Value = VectorCodes;

    const KIND: KeyKind = KeyKind::VectorCodes;

    fn encode(&self) -> Key {
        let mut bytes = Self::partition_prefix(&self.address, self.partition);
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let partition = reader.take_u32()?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            address,
            partition,
            id,
        })
    }
}

/// The product-quantized form of one record's vector: one code per subspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorCodes {
    /// One byte per subspace, naming the nearest codebook centroid.
    pub codes: Vec<u8>,
}

impl StoreValue for VectorCodes {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, self.codes.len());
        buffer.extend_from_slice(&self.codes);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        Ok(Self {
            codes: payload.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> IndexAddress {
        IndexAddress::new(
            tessari_types::NamespaceId::new(1),
            tessari_types::DatabaseId::new(2),
            tessari_types::TableId::new(3),
            tessari_types::IndexId::new(4),
        )
    }

    #[test]
    fn trained_state_round_trips() {
        let held = VectorTrained {
            partitions: 4,
            dimensions: 6,
            subspaces: 3,
            codebook_size: 256,
            centroids: (0..24).map(f64::from).collect(),
            codebooks: (0..3 * 256 * 2).map(|n| f64::from(n) * 0.5).collect(),
        };
        let back = VectorTrained::decode(held.encode().as_slice()).expect("decodes");
        assert_eq!(held, back);
        assert_eq!(back.subspace_width(), 2);
        assert_eq!(back.centroid(2).len(), 6);
        assert_eq!(back.codebook(1).len(), 512);
    }

    #[test]
    fn code_keys_round_trip_and_partition_sorts_before_records() {
        let at = address();
        let first = VectorCodeKey::new(at, 1, RecordId::Int(7));
        let second = VectorCodeKey::new(at, 2, RecordId::Int(1));
        assert!(first.encode() < second.encode());
        let back = VectorCodeKey::decode(first.encode().as_slice()).expect("decodes");
        assert_eq!(back.partition, 1);
        assert_eq!(back.id, RecordId::Int(7));
        let codes = VectorCodes {
            codes: vec![3, 250, 0],
        };
        assert_eq!(
            VectorCodes::decode(codes.encode().as_slice()).expect("decodes"),
            codes
        );
    }
}
