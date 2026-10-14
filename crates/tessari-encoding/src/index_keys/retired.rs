//! What a plain index entry said, kept after the commit that removed it (Q-S3-34).
//!
//! ```text
//! retired  <0x45> <ns:u32> <db:u32> <tb:u32> <ix:u32> <values> <0x00> <record-id> <retired-at:u64>
//! ```
//!
//! A secondary entry describes the committed tail and nothing else, so a
//! transaction whose snapshot is older cannot find through it a record that held
//! a value at the snapshot and has changed since. The commit that removes such an
//! entry therefore also writes this one: the same values and record under their
//! own tag, stamped with the version that removed them. A reader behind the tail
//! takes the retired entries whose stamp is newer than its snapshot as further
//! candidates, and re-tests every candidate at its own snapshot.
//!
//! Its own tag rather than a flag on the secondary entry, so that every reader of
//! the live entries — the walks, the statistics, the ordered reads — is untouched
//! by their existence. The stamp is in the KEY, so each removal is its own key and
//! reclaiming one below the floor can never take a newer one with it.

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
use tessari_types::{RecordId, Sequence};

/// One removed entry of a non-unique index, and the version that removed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredIndexKey {
    /// Which index the removed entry belonged to.
    pub address: IndexAddress,
    /// The values the removed entry held.
    pub values: IndexValues,
    /// The record the removed entry pointed at.
    pub id: RecordId,
    /// The version of the commit that removed it.
    pub retired_at: Sequence,
}

impl RetiredIndexKey {
    /// Build a retired entry key.
    #[must_use]
    pub const fn new(
        address: IndexAddress,
        values: IndexValues,
        id: RecordId,
        retired_at: Sequence,
    ) -> Self {
        Self {
            address,
            values,
            id,
            retired_at,
        }
    }
}

impl StoreKey for RetiredIndexKey {
    type Value = NoPayload;

    const KIND: KeyKind = KeyKind::RetiredIndex;

    fn encode(&self) -> Key {
        let mut bytes = self.address.prefix(Self::KIND);
        bytes.extend_from_slice(self.values.as_slice());
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        writer.put_u64(self.retired_at.get());
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let values = take_values(&mut reader)?;
        let id = record_id::take(&mut reader)?;
        let retired_at = Sequence::new(reader.take_u64()?);
        reader.finish()?;
        Ok(Self {
            address,
            values,
            id,
            retired_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessari_types::{DatabaseId, IndexId, NamespaceId, TableId, Value};

    fn address() -> IndexAddress {
        IndexAddress::new(
            NamespaceId::new(7),
            DatabaseId::new(8),
            TableId::new(9),
            IndexId::new(10),
        )
    }

    #[test]
    fn a_retired_entry_round_trips() -> Result<()> {
        let key = RetiredIndexKey::new(
            address(),
            IndexValues::of(&[Value::from("a")]),
            RecordId::from(3_i64),
            Sequence::new(42),
        );
        assert_eq!(RetiredIndexKey::decode(key.encode().as_slice())?, key);
        Ok(())
    }

    #[test]
    fn retired_entries_share_the_live_entries_value_order() {
        // A reader bounds both walks with the same value range, so the retired
        // keys of one value must sort inside that value's run, whatever the stamp.
        let low = RetiredIndexKey::new(
            address(),
            IndexValues::of(&[Value::from("a")]),
            RecordId::from(9_i64),
            Sequence::new(900),
        );
        let high = RetiredIndexKey::new(
            address(),
            IndexValues::of(&[Value::from("b")]),
            RecordId::from(1_i64),
            Sequence::new(1),
        );
        assert!(low.encode().as_slice() < high.encode().as_slice());
    }
}
