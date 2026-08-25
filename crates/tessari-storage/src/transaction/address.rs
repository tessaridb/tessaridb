//! Where a record lives, and the key ranges that reach it.
//!
//! An address is the tuple every other module in here starts from: the four ids
//! that name a record, and the two helpers that turn "everything under this
//! prefix" into a bounded range.

use tessari_encoding::RecordKey;
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

/// The first byte string after every key beginning with these bytes.
///
/// Incrementing the last byte that can be incremented, and dropping the trailing
/// `0xFF`s — the standard way to turn "everything with this prefix" into an
/// exclusive upper bound. All-`0xFF` bytes have no successor, and the answer is
/// then an empty vector, which `KeyRange::between` reads as unbounded above.
pub(super) fn after(mut bytes: Vec<u8>) -> Vec<u8> {
    while let Some(last) = bytes.pop() {
        if last != u8::MAX {
            bytes.push(last.saturating_add(1));
            return bytes;
        }
    }
    bytes
}

/// The smallest key strictly greater than this one.
///
/// Appending a zero byte, which is the immediate successor in byte order: a key
/// above `bytes` either extends it — and the shortest extension is this one — or
/// differs from it earlier, and is then above every extension of it. So a scan
/// resuming here sees every remaining key and re-reads none.
///
/// Deliberately **not** [`after`], which is the successor of the whole *prefix*
/// and skips every key carrying `bytes` as a byte prefix. Today no index key
/// carries another as a prefix, because every variable-width component of one is
/// terminated when it is encoded — so `after` would work here. That property
/// lives in the encoder, a crate away from this loop, and a walk that silently
/// returns fewer records if it ever changes is not worth the byte it saves.
pub(super) fn resuming_after(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.push(0);
    bytes
}

/// A record as an index read hands it back: its identity, and its stored bytes.
///
/// Named because [`Transaction::records_in_descending_order`] answers with an
/// *optional* list of them, and a nested triple of generics is a signature
/// nobody reads twice.
pub type StoredRecord = (RecordId, Vec<u8>);

/// Where a record lives: its table, and its identity within it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordAddress {
    /// The namespace.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The table within the database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
}

impl RecordAddress {
    /// Address a record.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: RecordId,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            id,
        }
    }

    pub(super) fn key_at(&self, version: Sequence) -> RecordKey {
        RecordKey::new(
            self.namespace,
            self.database,
            self.table,
            self.id.clone(),
            version,
        )
    }

    pub(super) fn versions_prefix(&self) -> Vec<u8> {
        RecordKey::versions_prefix(self.namespace, self.database, self.table, &self.id)
    }
}
