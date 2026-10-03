//! What a value index holds, summarised for the planner, and how much of it has
//! changed since.
//!
//! # A fact about this copy, and only about cost
//!
//! Both keys sit beside the index's entries and are derived from them, like
//! [`super::VectorRecallKey`]: the statistic is taken by walking this node's
//! entries and the change counter is kept where this node writes them. Neither
//! travels in the log, and neither decides which records any read returns — a
//! statistic that is stale, missing or wrong costs a slower path and never a
//! different answer, which is why a node is free to hold its own.
//!
//! Both keys are exactly an index prefix with no suffix, so one index has one
//! of each, finding either is a point read, and both are cleared with the
//! index's keyspace when its entries are.

use super::IndexAddress;
use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::value::{StoreValue, split_header, with_header};
use tessari_kv::{Key, Value};

/// Where one index's statistics are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatisticsKey {
    /// The index summarised.
    pub address: IndexAddress,
}

impl IndexStatisticsKey {
    /// The key for one index's statistics.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for IndexStatisticsKey {
    type Value = IndexStatistics;

    const KIND: KeyKind = KeyKind::IndexStatistics;

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

/// One value index, summarised from a walk of its entries.
///
/// Values are held in the index's own encoding — the leading bytes an entry
/// key carries — so a statistic compares with a lookup byte for byte and never
/// decodes a value to do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatistics {
    /// How many records the table held when the walk was taken.
    pub records: u64,
    /// The index's change counter when the walk was taken.
    pub changes: u64,
    /// How many entries the index held.
    pub entries: u64,
    /// How many distinct values each leading run of the index's fields held:
    /// the first field alone, the first two, and so on to the whole key.
    pub distinct: Vec<u64>,
    /// The most common complete values with how many entries each held, most
    /// common first.
    pub common: Vec<(Vec<u8>, u64)>,
    /// Equi-depth bucket bounds over the first field's values, ascending: each
    /// pair of neighbours holds about the same share of the entries.
    pub bounds: Vec<Vec<u8>>,
}

impl StoreValue for IndexStatistics {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u64(self.records)
            .put_u64(self.changes)
            .put_u64(self.entries);
        put_count(&mut writer, self.distinct.len());
        for held in &self.distinct {
            writer.put_u64(*held);
        }
        put_count(&mut writer, self.common.len());
        for (value, count) in &self.common {
            writer.put_variable(value).put_u64(*count);
        }
        put_count(&mut writer, self.bounds.len());
        for bound in &self.bounds {
            writer.put_variable(bound);
        }
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::IndexStatistics, payload);
        let records = reader.take_u64()?;
        let changes = reader.take_u64()?;
        let entries = reader.take_u64()?;
        let mut distinct = Vec::new();
        for _ in 0..reader.take_u32()? {
            distinct.push(reader.take_u64()?);
        }
        let mut common = Vec::new();
        for _ in 0..reader.take_u32()? {
            let value = reader.take_variable()?;
            common.push((value, reader.take_u64()?));
        }
        let mut bounds = Vec::new();
        for _ in 0..reader.take_u32()? {
            bounds.push(reader.take_variable()?);
        }
        reader.finish()?;
        Ok(Self {
            records,
            changes,
            entries,
            distinct,
            common,
            bounds,
        })
    }
}

/// A list's length as the four bytes it is written in.
///
/// Every list here is bounded by a constant far below `u32::MAX` (an index's
/// arity, the common values kept, the buckets), so the saturation is never
/// reached; it is a ceiling rather than a truncation.
fn put_count(writer: &mut KeyWriter, count: usize) {
    writer.put_u32(u32::try_from(count).unwrap_or(u32::MAX));
}

/// Where one index's change counter is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexChangesKey {
    /// The index counted.
    pub address: IndexAddress,
}

impl IndexChangesKey {
    /// The key for one index's change counter.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for IndexChangesKey {
    type Value = IndexChanges;

    const KIND: KeyKind = KeyKind::IndexChanges;

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

/// How many entries an index has gained or lost on this node, ever.
///
/// Only ever grows. A statistic records the value it was taken at, so what
/// has changed since is a subtraction, and no reset has to be coordinated with
/// the walk that takes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexChanges(pub u64);

impl StoreValue for IndexChanges {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u64(self.0);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::IndexChanges, payload);
        let changes = reader.take_u64()?;
        reader.finish()?;
        Ok(Self(changes))
    }
}

#[cfg(test)]
mod tests {
    use super::{IndexChanges, IndexStatistics};
    use crate::value::StoreValue;

    #[test]
    fn statistics_come_back_as_they_were_written() {
        let held = IndexStatistics {
            records: 50_000,
            changes: 12,
            entries: 49_998,
            distinct: vec![10, 997],
            common: vec![(vec![0x05, 0x00], 20_030), (vec![0x05, 0x01, 0x00], 31)],
            bounds: vec![vec![0x00, 0x01], vec![], vec![0xff, 0x00, 0x02]],
        };
        let bytes = held.encode();
        assert_eq!(IndexStatistics::decode(bytes.as_slice()).ok(), Some(held));
    }

    #[test]
    fn a_change_counter_comes_back_as_it_was_written() {
        let bytes = IndexChanges(u64::MAX - 3).encode();
        assert_eq!(
            IndexChanges::decode(bytes.as_slice()).ok(),
            Some(IndexChanges(u64::MAX - 3))
        );
    }
}
