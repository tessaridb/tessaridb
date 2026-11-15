//! What one declared field of one table held when it was last walked for the
//! planner, and how much of the table has changed since.
//!
//! # The index statistics' sibling, same rules
//!
//! Like [`super::IndexStatistics`], a field statistic is taken by walking this
//! node's copy, kept beside the data it describes in a keyspace derived from
//! the table address, travels in neither the log nor a backup, and decides a
//! cost and never an answer. The change counter is moved in the same commit
//! batch as the writes it counts, so a statistic's staleness is measured
//! against the same events that made it stale.
//!
//! Values are held in the field's order-preserving index encoding, so a
//! statistic compares with a lookup byte for byte and never decodes a value to
//! do it. The field path is kept in the key in its canonical text form, so the
//! key decodes without the catalog entry that describes it.

use tessari_kv::{Key, Value};
use tessari_types::{DatabaseId, NamespaceId, Path, TableId};

use crate::error::{Error, Result};
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::value::{StoreValue, split_header, with_header};

/// Which field, on which table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldAddress {
    /// The namespace.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The table the field is on.
    pub table: TableId,
    /// The field itself, in its canonical text form.
    pub field: Path,
}

impl FieldAddress {
    /// Name one field.
    #[must_use]
    pub fn new(namespace: NamespaceId, database: DatabaseId, table: TableId, field: Path) -> Self {
        Self {
            namespace,
            database,
            table,
            field,
        }
    }

    fn write_to(&self, writer: &mut KeyWriter) {
        writer
            .put_u32(self.namespace.get())
            .put_u32(self.database.get())
            .put_u32(self.table.get())
            .put_variable(self.field.to_string().as_bytes());
    }

    fn read_from(reader: &mut KeyReader<'_>) -> Result<Self> {
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let bytes = reader.take_variable()?;
        let text = core::str::from_utf8(&bytes).map_err(|_| Error::InvalidUtf8 {
            kind: KeyKind::FieldStatistics,
        })?;
        let field = Path::parse(text).ok_or(Error::InvalidUtf8 {
            kind: KeyKind::FieldStatistics,
        })?;
        Ok(Self {
            namespace,
            database,
            table,
            field,
        })
    }
}

/// Where one field's statistics are kept: the kind tag, the tenancy, and the
/// field path, so finding one is a point read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldStatisticsKey {
    /// The field summarised.
    pub address: FieldAddress,
}

impl FieldStatisticsKey {
    /// The key for one field's statistics.
    #[must_use]
    pub const fn new(address: FieldAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for FieldStatisticsKey {
    type Value = FieldStatistics;

    const KIND: KeyKind = KeyKind::FieldStatistics;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::new();
        writer.put_u8(Self::KIND.tag());
        self.address.write_to(&mut writer);
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = FieldAddress::read_from(&mut reader)?;
        reader.finish()?;
        Ok(Self { address })
    }
}

/// One declared field of one table, summarised from a walk of its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldStatistics {
    /// How many records the table held when the walk was taken.
    pub records: u64,
    /// The table's change counter when the walk was taken.
    pub changes: u64,
    /// How many records held the field with a value.
    pub entries: u64,
    /// How many records held the field resolved to null.
    pub nulls: u64,
    /// How many distinct values the field held.
    pub distinct: u64,
    /// The smallest value held, in the field's order-preserving encoding;
    /// absent when every record was null or empty of the field.
    pub low: Option<Vec<u8>>,
    /// The largest value held, in the same encoding.
    pub high: Option<Vec<u8>>,
    /// The most common values with how many records each held, most common
    /// first.
    pub common: Vec<(Vec<u8>, u64)>,
    /// Equi-depth bucket bounds over the field's values, ascending: each pair
    /// of neighbours holds about the same share of the entries.
    pub bounds: Vec<Vec<u8>>,
}

impl StoreValue for FieldStatistics {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u64(self.records)
            .put_u64(self.changes)
            .put_u64(self.entries)
            .put_u64(self.nulls)
            .put_u64(self.distinct);
        put_optional(&mut writer, &self.low);
        put_optional(&mut writer, &self.high);
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
        let mut reader = KeyReader::new(KeyKind::FieldStatistics, payload);
        let records = reader.take_u64()?;
        let changes = reader.take_u64()?;
        let entries = reader.take_u64()?;
        let nulls = reader.take_u64()?;
        let distinct = reader.take_u64()?;
        let low = take_optional(&mut reader)?;
        let high = take_optional(&mut reader)?;
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
            nulls,
            distinct,
            low,
            high,
            common,
            bounds,
        })
    }
}

/// Where one table's field-statistics change counter is kept: the kind tag
/// and the tenancy, with no field — one counter counts every write to the
/// table, for all its fields at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldChangesKey {
    /// The table counted.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The table counted.
    pub table: TableId,
}

impl FieldChangesKey {
    /// The key for one table's change counter.
    #[must_use]
    pub const fn new(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Self {
        Self {
            namespace,
            database,
            table,
        }
    }
}

impl StoreKey for FieldChangesKey {
    type Value = FieldChanges;

    const KIND: KeyKind = KeyKind::FieldChanges;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::new();
        writer
            .put_u8(Self::KIND.tag())
            .put_u32(self.namespace.get())
            .put_u32(self.database.get())
            .put_u32(self.table.get());
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
        })
    }
}

/// How many writes one table has gained or lost on this node, ever.
///
/// Only ever grows, like [`super::IndexChanges`]: a statistic records the
/// value it was taken at, so what has changed since is a subtraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldChanges(pub u64);

impl StoreValue for FieldChanges {
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
        let mut reader = KeyReader::new(KeyKind::FieldChanges, payload);
        let changes = reader.take_u64()?;
        reader.finish()?;
        Ok(Self(changes))
    }
}

/// An optional value as one variable field: the encoding of a value is never
/// empty, so an empty field means `None`.
fn put_optional(writer: &mut KeyWriter, held: &Option<Vec<u8>>) {
    match held {
        Some(bytes) => writer.put_variable(bytes),
        None => writer.put_variable(&[]),
    };
}

fn take_optional(reader: &mut KeyReader<'_>) -> Result<Option<Vec<u8>>> {
    let bytes = reader.take_variable()?;
    Ok((!bytes.is_empty()).then_some(bytes))
}

/// A list's length as the four bytes it is written in, the same ceiling the
/// index statistics use.
fn put_count(writer: &mut KeyWriter, count: usize) {
    writer.put_u32(u32::try_from(count).unwrap_or(u32::MAX));
}

#[cfg(test)]
mod tests {
    use super::{FieldChanges, FieldStatistics, FieldStatisticsKey};
    use crate::index_keys::FieldAddress;
    use crate::keys::StoreKey;
    use crate::value::StoreValue;
    use tessari_types::{DatabaseId, NamespaceId, Path, TableId};

    fn address() -> FieldAddress {
        FieldAddress::new(
            NamespaceId::new(7),
            DatabaseId::new(3),
            TableId::new(11),
            Path::field("city"),
        )
    }

    #[test]
    fn a_key_round_trips_with_its_field_path() {
        let key = FieldStatisticsKey::new(address());
        let bytes = key.encode();
        assert_eq!(FieldStatisticsKey::decode(bytes.as_slice()).ok(), Some(key));
    }

    #[test]
    fn statistics_come_back_as_they_were_written() {
        let held = FieldStatistics {
            records: 50_000,
            changes: 12,
            entries: 49_900,
            nulls: 80,
            distinct: 997,
            low: Some(vec![0x00, 0x01]),
            high: Some(vec![0xff, 0x00]),
            common: vec![(vec![0x05, 0x00], 20_030), (vec![0x05, 0x01], 31)],
            bounds: vec![vec![0x00, 0x01], vec![0x80], vec![0xff, 0x00]],
        };
        let bytes = held.encode();
        assert_eq!(FieldStatistics::decode(bytes.as_slice()).ok(), Some(held));
    }

    #[test]
    fn empty_bounds_round_trip_as_no_value() {
        let held = FieldStatistics {
            records: 10,
            changes: 0,
            entries: 0,
            nulls: 10,
            distinct: 0,
            low: None,
            high: None,
            common: Vec::new(),
            bounds: Vec::new(),
        };
        let bytes = held.encode();
        assert_eq!(FieldStatistics::decode(bytes.as_slice()).ok(), Some(held));
    }

    #[test]
    fn a_change_counter_comes_back_as_it_was_written() {
        let bytes = FieldChanges(u64::MAX - 3).encode();
        assert_eq!(
            FieldChanges::decode(bytes.as_slice()).ok(),
            Some(FieldChanges(u64::MAX - 3))
        );
    }
}
