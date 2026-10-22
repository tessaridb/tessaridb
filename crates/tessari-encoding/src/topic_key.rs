//! The two indexes of a topic's order (G037).
//!
//! ```text
//! <0x1c> <namespace:u32> <database:u32> <table:u32> <offset:u64> <record-id> → [appended ms]
//! <0x1d> <namespace:u32> <database:u32> <table:u32> <record-id> <offset:u64>
//! <0x1e> <namespace:u32> <database:u32> <table:u32>        → last position given
//! ```
//!
//! Every message of a topic holds one position, dense from 1 in the order the
//! messages were committed. The first index answers *what comes after position
//! n* with a forward read from n; the second answers *which position does this
//! message hold*, which is what removing a message needs, since the message
//! itself does not carry it. Both are written in the message's own batch. The
//! second holds nothing — its presence is the whole statement — and the first
//! holds the instant the message was appended, or nothing for one appended
//! before its topic kept the instant (ADR-0133 D3).
//!
//! The third holds the last position the topic has given, and is never
//! removed. It is not the last entry of the first index: retention may remove
//! every message, and a topic that then started again from 1 would hand a
//! reader positions it has already passed.

use tessari_kv::{Key, Value};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::{Error, Result};
use crate::expiry_key::ExpiryMark;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};

/// One message of a topic, filed by its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicOffsetKey {
    /// The namespace the topic belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The topic.
    pub table: TableId,
    /// The message's position, from 1.
    pub offset: u64,
    /// The message.
    pub id: RecordId,
}

/// One message of a topic, filed by its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicEntryKey {
    /// The namespace the topic belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The topic.
    pub table: TableId,
    /// The message.
    pub id: RecordId,
    /// The message's position, from 1.
    pub offset: u64,
}

/// The last position one topic has given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicHeadKey {
    /// The namespace the topic belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The topic.
    pub table: TableId,
}

impl StoreKey for TopicHeadKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::TopicHead;

    fn encode(&self) -> Key {
        Key::from(prefix(
            KeyKind::TopicHead,
            self.namespace,
            self.database,
            self.table,
        ))
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

/// How many payload bytes one size-retained topic holds (G055 C8).
///
/// Kept only for a topic declared `RETAIN BYTES`, so its count begins with its
/// first message and is complete; an absent count is recounted from the
/// messages rather than read as zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicBytesKey {
    /// The namespace the topic belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The topic.
    pub table: TableId,
}

impl StoreKey for TopicBytesKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::TopicBytes;

    fn encode(&self) -> Key {
        Key::from(prefix(
            KeyKind::TopicBytes,
            self.namespace,
            self.database,
            self.table,
        ))
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

fn prefix(kind: KeyKind, namespace: NamespaceId, database: DatabaseId, table: TableId) -> Vec<u8> {
    let mut writer = KeyWriter::with_capacity(13);
    writer
        .put_u8(kind.tag())
        .put_u32(namespace.get())
        .put_u32(database.get())
        .put_u32(table.get());
    writer.finish()
}

impl TopicOffsetKey {
    /// The prefix every position of one topic shares, first position first.
    #[must_use]
    pub fn table_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Vec<u8> {
        prefix(KeyKind::TopicOffset, namespace, database, table)
    }

    /// The key a forward read starting at `offset` seeks to.
    #[must_use]
    pub fn from_offset(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        offset: u64,
    ) -> Vec<u8> {
        let mut bytes = Self::table_prefix(namespace, database, table);
        let mut writer = KeyWriter::with_capacity(8);
        writer.put_u64(offset);
        bytes.extend_from_slice(&writer.finish());
        bytes
    }
}

impl TopicEntryKey {
    /// The prefix every message of one topic is filed under.
    #[must_use]
    pub fn table_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Vec<u8> {
        prefix(KeyKind::TopicEntry, namespace, database, table)
    }

    /// The prefix under which one message's single entry sits.
    #[must_use]
    pub fn message_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: &RecordId,
    ) -> Vec<u8> {
        let mut bytes = prefix(KeyKind::TopicEntry, namespace, database, table);
        let mut writer = KeyWriter::with_capacity(16);
        record_id::put(&mut writer, id);
        bytes.extend_from_slice(&writer.finish());
        bytes
    }
}

/// What a position's entry holds: the instant its message was appended, in
/// milliseconds since the epoch (ADR-0133 D3).
///
/// The header alone — every position written before the instant was kept — is
/// a mark with no instant, so the entries already on disk read unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopicMark {
    /// When the message was appended, or `None` for one appended before a
    /// topic kept the instant.
    pub appended: Option<u64>,
}

impl StoreValue for TopicMark {
    fn encode(&self) -> Value {
        let Some(appended) = self.appended else {
            return ExpiryMark.encode();
        };
        let mut bytes = with_header(0, 8);
        bytes.extend_from_slice(&appended.to_be_bytes());
        Value::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        if payload.is_empty() {
            return Ok(Self { appended: None });
        }
        let instant: [u8; 8] = payload
            .get(..8)
            .and_then(|head| head.try_into().ok())
            .ok_or(Error::ValueTruncated {
                len: bytes.len(),
                needed: bytes.len().saturating_add(8).saturating_sub(payload.len()),
            })?;
        if payload.len() > 8 {
            return Err(Error::TrailingBytes {
                kind: KeyKind::TopicOffset,
                extra: payload.len().saturating_sub(8),
            });
        }
        Ok(Self {
            appended: Some(u64::from_be_bytes(instant)),
        })
    }
}

impl StoreKey for TopicOffsetKey {
    type Value = TopicMark;

    const KIND: KeyKind = KeyKind::TopicOffset;

    fn encode(&self) -> Key {
        let mut bytes = Self::from_offset(self.namespace, self.database, self.table, self.offset);
        let mut writer = KeyWriter::with_capacity(16);
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let offset = reader.take_u64()?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            offset,
            id,
        })
    }
}

impl StoreKey for TopicEntryKey {
    type Value = ExpiryMark;

    const KIND: KeyKind = KeyKind::TopicEntry;

    fn encode(&self) -> Key {
        let mut bytes = Self::message_prefix(self.namespace, self.database, self.table, &self.id);
        let mut writer = KeyWriter::with_capacity(8);
        writer.put_u64(self.offset);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let id = record_id::take(&mut reader)?;
        let offset = reader.take_u64()?;
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            id,
            offset,
        })
    }
}

#[cfg(test)]
mod tests;
