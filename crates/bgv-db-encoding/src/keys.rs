//! Typed store keys.
//!
//! Each key type declares, in the type system, which value type it addresses. A
//! shared `get(key) -> bytes` helper cannot do that, and the resulting mistake —
//! decoding one entity's bytes as another type — surfaces far away from the code
//! that caused it.
//!
//! Keys carry identity and nothing else. Names live in the catalog, so renaming
//! a namespace, database or table rewrites one catalog entry rather than every
//! record and every index entry that mentions it.

use bgv_db_kv::{Key, Keyspace};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::Result;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{FormatVersion, StoreValue};

/// Bytes of a record key that identify the table: the kind tag plus the three
/// tenancy identifiers.
///
/// This is a fixed width on purpose. A meaningful prefix has to be a fixed
/// leading byte count for a prefix filter to be able to extract it, and that
/// constraint is the whole reason the identifiers are numbers rather than names.
pub const TABLE_PREFIX_LEN: usize = 13;

/// A key that addresses one kind of stored value.
pub trait StoreKey: Sized {
    /// The value type stored under this key.
    type Value: StoreValue;

    /// The kind tag this key carries.
    const KIND: KeyKind;

    /// Encode to the bytes used in the substrate.
    fn encode(&self) -> Key;

    /// Decode from bytes read out of the substrate.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes carry a different kind tag, are
    /// truncated, hold an unterminated component, or have trailing bytes.
    fn decode(bytes: &[u8]) -> Result<Self>;

    /// The keyspace this key lives in.
    #[must_use]
    fn keyspace() -> Keyspace {
        Self::KIND.keyspace()
    }
}

/// Addresses one version of one record.
///
/// ```text
/// <0x01> <namespace:u32> <database:u32> <table:u32> <record-id> <!version:u64>
/// ```
///
/// The version suffix is the complement of the sequence, so the versions of one
/// record sort newest-first and a snapshot read is a seek followed by taking the
/// first entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordKey {
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
    /// The sequence at which this version was written.
    pub version: Sequence,
}

impl RecordKey {
    /// Build a record key.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: RecordId,
        version: Sequence,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            id,
            version,
        }
    }

    /// The prefix shared by every record in one table.
    ///
    /// Always [`TABLE_PREFIX_LEN`] bytes long.
    #[must_use]
    pub fn table_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(TABLE_PREFIX_LEN);
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get());
        writer.finish()
    }

    /// The prefix shared by every version of one record.
    ///
    /// Bounding a scan with this is what keeps a snapshot read inside the record
    /// it asked for instead of running on into the next one.
    #[must_use]
    pub fn versions_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: &RecordId,
    ) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(TABLE_PREFIX_LEN.saturating_add(24));
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get());
        record_id::put(&mut writer, id);
        writer.finish()
    }
}

impl StoreKey for RecordKey {
    type Value = crate::value::RecordValue;

    const KIND: KeyKind = KeyKind::Record;

    fn encode(&self) -> Key {
        let mut bytes = Self::versions_prefix(self.namespace, self.database, self.table, &self.id);
        bytes.extend_from_slice(&(!self.version.get()).to_be_bytes());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let id = record_id::take(&mut reader)?;
        let version = Sequence::new(reader.take_u64_descending()?);
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            id,
            version,
        })
    }
}

/// Addresses the store's own on-disk format version.
///
/// A singleton, written at creation and read at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatVersionKey;

impl StoreKey for FormatVersionKey {
    type Value = FormatVersion;

    const KIND: KeyKind = KeyKind::FormatVersion;

    fn encode(&self) -> Key {
        Key::from(vec![Self::KIND.tag()])
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        reader.finish()?;
        Ok(Self)
    }
}

/// Addresses the log position whose effects are durably present in the state.
///
/// Written in the same batch as the state it describes, which is what turns
/// recovery into a resumable replay instead of a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppliedPositionKey;

impl StoreKey for AppliedPositionKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::AppliedPosition;

    fn encode(&self) -> Key {
        Key::from(vec![Self::KIND.tag()])
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        reader.finish()?;
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::error::Error;

    fn key(id: RecordId, version: u64) -> RecordKey {
        RecordKey::new(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            id,
            Sequence::new(version),
        )
    }

    #[test]
    fn a_record_key_round_trips_every_id_variant() {
        let ids = [
            RecordId::Int(-42),
            RecordId::Int(i64::MAX),
            RecordId::from("user"),
            RecordId::from(""),
            RecordId::Uuid([0x5a; 16]),
            RecordId::Bytes(vec![0x00, 0xff, 0x00]),
        ];
        for id in ids {
            let original = key(id, 7);
            let encoded = original.encode();
            assert_eq!(RecordKey::decode(encoded.as_slice()).unwrap(), original);
        }
    }

    #[test]
    fn the_table_prefix_is_fixed_width_and_leads_every_record_key() {
        let prefix =
            RecordKey::table_prefix(NamespaceId::new(1), DatabaseId::new(2), TableId::new(3));
        assert_eq!(prefix.len(), TABLE_PREFIX_LEN);
        let encoded = key(RecordId::from("x"), 1).encode();
        assert!(encoded.as_slice().starts_with(&prefix));
    }

    #[test]
    fn every_version_of_a_record_shares_the_versions_prefix() {
        let id = RecordId::from("same");
        let prefix = RecordKey::versions_prefix(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            &id,
        );
        for version in [0, 1, u64::MAX] {
            let encoded = key(id.clone(), version).encode();
            assert!(encoded.as_slice().starts_with(&prefix));
            assert_eq!(encoded.len(), prefix.len().saturating_add(8));
        }
    }

    #[test]
    fn newer_versions_of_a_record_sort_first() {
        let older = key(RecordId::from("r"), 5).encode();
        let newer = key(RecordId::from("r"), 9).encode();
        assert!(newer.as_slice() < older.as_slice());
    }

    #[test]
    fn distinct_records_stay_ordered_despite_the_version_suffix() {
        // Without a terminator on the record id, "a" followed by a version whose
        // first byte exceeds 'b' would sort after "ab". The version chosen here
        // is the one that would trigger it.
        let short = key(RecordId::from("a"), !0x6200_0000_0000_0000_u64).encode();
        let long = key(RecordId::from("ab"), 0).encode();
        assert!(short.as_slice() < long.as_slice());
    }

    #[test]
    fn decoding_a_record_key_as_a_meta_key_is_refused() {
        let encoded = key(RecordId::Int(1), 1).encode();
        let error = FormatVersionKey::decode(encoded.as_slice()).unwrap_err();
        assert!(matches!(error, Error::UnexpectedKind { .. }));
    }

    #[test]
    fn singleton_meta_keys_are_one_byte_and_round_trip() {
        let format = FormatVersionKey.encode();
        assert_eq!(format.len(), 1);
        assert_eq!(
            FormatVersionKey::decode(format.as_slice()).unwrap(),
            FormatVersionKey
        );

        let applied = AppliedPositionKey.encode();
        assert_eq!(applied.len(), 1);
        assert_eq!(
            AppliedPositionKey::decode(applied.as_slice()).unwrap(),
            AppliedPositionKey
        );
        assert_ne!(format.as_slice(), applied.as_slice());
    }

    #[test]
    fn a_singleton_key_with_trailing_bytes_is_refused() {
        let bytes = [KeyKind::FormatVersion.tag(), 0x00];
        assert!(matches!(
            FormatVersionKey::decode(&bytes).unwrap_err(),
            Error::TrailingBytes { extra: 1, .. }
        ));
    }

    #[test]
    fn each_key_type_reports_its_keyspace() {
        assert_eq!(RecordKey::keyspace(), Keyspace::DATA);
        assert_eq!(FormatVersionKey::keyspace(), Keyspace::META);
        assert_eq!(AppliedPositionKey::keyspace(), Keyspace::META);
    }
}
