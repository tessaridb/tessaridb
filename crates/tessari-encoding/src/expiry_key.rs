//! The expiry index: when each expiring record version stops being answered
//! (G035).
//!
//! ```text
//! <0x1a> <at:u64> <namespace:u32> <database:u32> <table:u32> <record-id>
//! ```
//!
//! The instant leads, so every entry that has passed is one forward scan from
//! the start of the kind up to the clock — the removal pass never tests an entry
//! that has not expired, and never walks a table to find the few that have.
//! Nothing is stored under the key: its presence is the whole statement.

use tessari_kv::{Key, KeyRange, Value};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};

/// One expiring record version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiryKey {
    /// The millisecond since the Unix epoch the version stops being answered at.
    pub at: u64,
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record.
    pub id: RecordId,
}

impl ExpiryKey {
    /// Every entry whose instant is at or before `now`.
    #[must_use]
    pub fn passed_by(now: u64) -> KeyRange {
        let start = vec![KeyKind::ExpiryIndex.tag()];
        let mut end = KeyWriter::with_capacity(9);
        end.put_u8(KeyKind::ExpiryIndex.tag());
        match now.checked_add(1) {
            Some(past) => {
                end.put_u64(past);
                KeyRange::between(Key::from(start), Key::from(end.finish()))
            }
            None => KeyRange::prefix(&start),
        }
    }
}

impl StoreKey for ExpiryKey {
    type Value = ExpiryMark;

    const KIND: KeyKind = KeyKind::ExpiryIndex;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::with_capacity(32);
        writer
            .put_u8(Self::KIND.tag())
            .put_u64(self.at)
            .put_u32(self.namespace.get())
            .put_u32(self.database.get())
            .put_u32(self.table.get());
        record_id::put(&mut writer, &self.id);
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let at = reader.take_u64()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            at,
            namespace,
            database,
            table,
            id,
        })
    }
}

/// What an expiry-index entry holds: nothing beyond the common header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiryMark;

impl StoreValue for ExpiryMark {
    fn encode(&self) -> Value {
        Value::from(with_header(0, 0))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        split_header(bytes, 0)?;
        Ok(Self)
    }
}

#[cfg(test)]
mod tests;
