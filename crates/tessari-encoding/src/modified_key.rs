//! The modified-order index of a limited space (G036).
//!
//! ```text
//! <0x1b> <namespace:u32> <database:u32> <table:u32> <version:u64> <record-id>
//! ```
//!
//! One entry per stored key of a space that declared a limit, at the version
//! the key was last written at, so the table's least recently modified keys are
//! the first entries under its prefix and choosing what to evict is a bounded
//! forward read rather than a walk of the space. Nothing is stored under the
//! key: its presence is the whole statement, as for the expiry index.

use tessari_kv::Key;
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::Result;
use crate::expiry_key::ExpiryMark;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;

/// One key of a limited space, at the version it was last written at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifiedKey {
    /// The namespace the space belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The space.
    pub table: TableId,
    /// The version the key was last written at, on this node.
    pub version: Sequence,
    /// The key.
    pub id: RecordId,
}

impl ModifiedKey {
    /// The prefix every entry of one space shares, oldest first beneath it.
    #[must_use]
    pub fn table_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(13);
        writer
            .put_u8(KeyKind::ModifiedOrder.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get());
        writer.finish()
    }
}

impl StoreKey for ModifiedKey {
    type Value = ExpiryMark;

    const KIND: KeyKind = KeyKind::ModifiedOrder;

    fn encode(&self) -> Key {
        let mut bytes = Self::table_prefix(self.namespace, self.database, self.table);
        let mut writer = KeyWriter::with_capacity(24);
        writer.put_u64(self.version.get());
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
        let version = Sequence::new(reader.take_u64()?);
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            version,
            id,
        })
    }
}

#[cfg(test)]
mod tests;
