//! One entry of a containment index (ADR-0116 D4).
//!
//! ```text
//! containment  <0x44> <ns:u32> <db:u32> <tb:u32> <ix:u32> <path> <leaf> <0x00> <record-id>
//! ```
//!
//! The shape of a secondary entry under its own tag, with exactly two values:
//! the **path** to a leaf of the indexed document — an array of steps, a field
//! name for a field and `NULL` for every element of an array — and the **leaf**
//! itself. A record writes one entry per leaf its document holds, and a
//! containment read walks one leaf asked for at a time.
//!
//! Its own tag rather than the secondary one, because the entries mean something
//! an ordered index's do not: a build that read the catalog without knowing this
//! kind would otherwise take them for an ordered index on the field and serve an
//! equality from them.

use super::IndexAddress;
use super::IndexValues;
use super::NoPayload;
use super::posting::take_values;
use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use tessari_kv::Key;
use tessari_types::RecordId;

/// One (path, leaf) pair one record holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentKey {
    /// Which index this entry belongs to.
    pub address: IndexAddress,
    /// The path and the leaf, encoded as two index values.
    pub values: IndexValues,
    /// The record the entry points at.
    pub id: RecordId,
}

impl ContainmentKey {
    /// Build an entry key.
    #[must_use]
    pub const fn new(address: IndexAddress, values: IndexValues, id: RecordId) -> Self {
        Self {
            address,
            values,
            id,
        }
    }

    /// The bytes every entry for one (path, leaf) pair shares: the index's own
    /// prefix and the pair, so a scan over them is every record holding it.
    #[must_use]
    pub fn pair_prefix(address: &IndexAddress, values: &IndexValues) -> Vec<u8> {
        let mut bytes = address.prefix(Self::KIND);
        bytes.extend_from_slice(values.as_slice());
        bytes
    }
}

impl StoreKey for ContainmentKey {
    type Value = NoPayload;

    const KIND: KeyKind = KeyKind::Containment;

    fn encode(&self) -> Key {
        let mut bytes = Self::pair_prefix(&self.address, &self.values);
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let values = take_values(&mut reader)?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            address,
            values,
            id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessari_types::{DatabaseId, IndexId, NamespaceId, Number, TableId, Value};

    #[test]
    fn an_entry_reads_back_and_its_pair_is_a_prefix_of_it() {
        let address = IndexAddress::new(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            IndexId::new(4),
        );
        let path = Value::Array(vec![Value::from("lines"), Value::Null, Value::from("sku")]);
        let values = IndexValues::of(&[path, Value::from("b")]);
        let key = ContainmentKey::new(address, values.clone(), RecordId::Int(7));
        let encoded = key.encode();
        assert_eq!(ContainmentKey::decode(encoded.as_slice()).ok(), Some(key));
        assert!(
            encoded
                .as_slice()
                .starts_with(&ContainmentKey::pair_prefix(&address, &values))
        );
        // A different leaf on the same path is a different run.
        let other = IndexValues::of(&[
            Value::Array(vec![Value::from("lines"), Value::Null, Value::from("sku")]),
            Value::Number(Number::Integer(1)),
        ]);
        assert!(
            !encoded
                .as_slice()
                .starts_with(&ContainmentKey::pair_prefix(&address, &other))
        );
    }
}
