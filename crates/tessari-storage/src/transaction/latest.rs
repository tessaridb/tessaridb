//! The newest record per value an index holds (ADR-0088 §3).
//!
//! An index entry is its values followed by the record's identity, and a series'
//! identity is a UUID version 7 — its time. So within one value the **last**
//! entry is the newest record, and the whole answer is one reverse seek per
//! distinct value: land on the last entry below the upper bound, take it, and
//! make that value's first possible key the next bound. The records between the
//! newest and the value's first entry are never read.

use tessari_encoding::{
    IndexAddress, IndexTarget, KeyKind, SecondaryIndexKey, StoreKey, StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};

use super::address::after;
use super::{RecordAddress, StoredRecord, Transaction};
use crate::catalog::IndexDefinition;
use crate::error::Result;

impl Transaction<'_> {
    /// For every value `index` holds, the record under its last entry — the
    /// newest, on a table whose identities are times — least value first.
    ///
    /// # Entries are taken at face value, and that is a precondition
    ///
    /// The same one [`Transaction::records_in_descending_order`] states: an
    /// entry's position is the answer, so its caller admits this walk only
    /// where every entry reflects the record it points at — the committed tail,
    /// no write of this transaction to the table.
    ///
    /// A record the read cannot answer with (past a series floor) hides its
    /// value entirely: the value's other entries are older still.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key cannot be decoded.
    pub fn newest_per_value(&self, index: &IndexDefinition) -> Result<Vec<StoredRecord>> {
        let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
        let kind = if index.unique {
            KeyKind::UniqueIndex
        } else {
            KeyKind::SecondaryIndex
        };
        let prefix = address.prefix(kind);
        let lower = Key::from(prefix.clone());
        let mut upper = Key::from(after(prefix));
        let mut found = Vec::new();
        loop {
            let request = ScanRequest {
                keyspace: kind.keyspace(),
                range: KeyRange::between(lower.clone(), upper.clone()),
                direction: ScanDirection::Reverse,
                limit: Some(1),
            };
            let batch = self.store.backend().scan(&request)?;
            let Some((key, value)) = batch.first() else {
                break;
            };
            let id = if index.unique {
                // A unique entry is the whole group: the next value is below it.
                upper = key.clone();
                IndexTarget::decode(value.as_slice())?.id
            } else {
                let entry = SecondaryIndexKey::decode(key.as_slice())?;
                // Every entry of this value sorts at or above its values prefix,
                // so an exclusive bound there skips the rest of the group.
                upper = Key::from(SecondaryIndexKey::values_prefix(&address, &entry.values));
                entry.id
            };
            let record = RecordAddress::new(index.namespace, index.database, index.table, id);
            let held = self.get_each(std::slice::from_ref(&record))?;
            if let Some(Some(payload)) = held.into_iter().next() {
                found.push((record.id, payload));
            }
        }
        found.reverse();
        Ok(found)
    }
}
