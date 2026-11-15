//! The columnar projection's keys: chunks, the per-record map, and the
//! open-chunk pointer.
//!
//! All three ride the record keyspace, after the table prefix, where a
//! record id would sit — distinguished by tags that share no value with a
//! record id discriminant (`0x01`..`0x04`), so one prefix scan never meets
//! two shapes (SG1 worksheet §4.2):
//!
//! ```text
//! <0x01> <namespace:u32> <database:u32> <table:u32>
//!     'C' <family> <chunk-seq:u64> <!version:u64>   one chunk version
//!     'M' <record-id> <!version:u64>                one record's chunk map
//!     'P' <family> <!version:u64>                   the family's open chunk seq
//!
//! family := 'I' <field-id:u32>          a declared field: the catalog's
//!                                       stable identity, renames do not re-key
//!           | 'N' <name:variable>       a schemaless field: the name is the
//!                                       identity, and schemaless fields have
//!                                       no rename surface to break it (D-SG2-8)
//! ```
//!
//! The version suffix is the complement of the sequence, exactly as on a
//! record key: versions of one chunk sort newest-first and a read at a
//! snapshot is a seek followed by taking the first entry. A rewritten chunk
//! is a new version of its key — snapshot reads see the layout as of the
//! snapshot (ADR-0137, worksheet §4.2).
//!
//! # The values under these keys are headerless
//!
//! A record version carries the versioned envelope (tombstone flag, stamp)
//! because records are deleted and resolved through it. Chunks, map entries
//! and pointers are never tombstoned of their own — a chunk is rewritten
//! whole (copy-on-write at chunk granularity, §5.2) and its newest version
//! at a snapshot is the only one a reader asks for — so their bytes carry
//! no envelope, and the columnar subsystem owns their interior.

use tessari_types::{DatabaseId, FieldId, NamespaceId, RecordId, Sequence, TableId};

use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;

use super::keys::{RecordKey, StoreKey, TABLE_PREFIX_LEN};
use tessari_kv::Key;

/// The position tags the columnar families carry, listed for the format
/// surface. Values here are chosen to keep the record-id discriminants
/// company without colliding with them.
pub(crate) const SURFACE: &[(u8, &str)] = &[
    (TAG_CHUNK, "columnar chunk"),
    (TAG_MAP, "columnar record map"),
    (TAG_POINTER, "columnar open-chunk pointer"),
    (TAG_FIELD_ID, "columnar field by id"),
    (TAG_FIELD_NAME, "columnar field by name"),
];

const TAG_CHUNK: u8 = b'C';
const TAG_MAP: u8 = b'M';
const TAG_POINTER: u8 = b'P';
const TAG_FIELD_ID: u8 = b'I';
const TAG_FIELD_NAME: u8 = b'N';

/// Which column a chunk family holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColumnFamily {
    /// A declared field, keyed by the catalog's stable field id — the name
    /// resolves at the catalog, the id rides on disk, and a rename does not
    /// re-key the chunks (worksheet §4.2).
    Id(FieldId),
    /// A schemaless field, keyed by its name. D-SG2-8: a schemaless field
    /// has no catalog identity to spend and no rename surface to survive,
    /// so the name is the stable identity, and a new name starts its own
    /// chunk family on first sight (worksheet §9, D-SG2-2).
    Name(String),
}

impl ColumnFamily {
    /// Append the family to a key or value under construction.
    fn put(&self, writer: &mut KeyWriter) {
        match self {
            Self::Id(id) => {
                writer.put_u8(TAG_FIELD_ID).put_u32(id.get());
            }
            Self::Name(name) => {
                writer.put_u8(TAG_FIELD_NAME).put_variable(name.as_bytes());
            }
        }
    }

    /// Read the family back.
    fn take(reader: &mut KeyReader<'_>) -> crate::Result<Self> {
        match reader.take_u8()? {
            TAG_FIELD_ID => Ok(Self::Id(FieldId::new(reader.take_u32()?))),
            TAG_FIELD_NAME => {
                let bytes = reader.take_variable()?;
                let name = String::from_utf8(bytes).map_err(|_| crate::Error::InvalidUtf8 {
                    kind: reader.kind(),
                })?;
                Ok(Self::Name(name))
            }
            found => Err(crate::Error::UnknownColumnarFamily {
                kind: reader.kind(),
                found,
            }),
        }
    }

    /// Append the family to a value's bytes (a record's map entry holds its
    /// families in value form, not key form).
    #[must_use]
    pub fn into_value_bytes(self) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(16);
        self.put(&mut writer);
        writer.finish()
    }

    /// Read the family out of a value's bytes.
    pub fn from_value_bytes(bytes: &[u8]) -> crate::Result<Self> {
        let mut reader = KeyReader::new(KeyKind::Record, bytes);
        let family = Self::take(&mut reader)?;
        reader.finish()?;
        Ok(family)
    }
}

/// One version of one field's chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkKey {
    /// The namespace the table belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// Which column the chunk holds.
    pub family: ColumnFamily,
    /// The chunk's sequence within its family — append order.
    pub seq: u64,
    /// The sequence at which this version was written.
    pub version: Sequence,
}

impl ChunkKey {
    /// Build a chunk key.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        family: ColumnFamily,
        seq: u64,
        version: Sequence,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            family,
            seq,
            version,
        }
    }

    /// The prefix under which every version of this chunk sorts.
    #[must_use]
    pub fn versions_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        family: &ColumnFamily,
        seq: u64,
    ) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(40);
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get())
            .put_u8(TAG_CHUNK);
        family.put(&mut writer);
        writer.put_u64(seq);
        writer.finish()
    }

    /// The prefix under which every chunk of the family sorts, in append
    /// order — the span a columnar scan reads.
    #[must_use]
    pub fn family_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        family: &ColumnFamily,
    ) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(40);
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get())
            .put_u8(TAG_CHUNK);
        family.put(&mut writer);
        writer.finish()
    }
}

impl StoreKey for ChunkKey {
    type Value = tessari_kv::Value;
    const KIND: KeyKind = KeyKind::Record;

    fn encode(&self) -> Key {
        let mut bytes = Self::versions_prefix(
            self.namespace,
            self.database,
            self.table,
            &self.family,
            self.seq,
        );
        bytes.extend_from_slice(&(!self.version.get()).to_be_bytes());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> crate::Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let tag = reader.take_u8()?;
        if tag != TAG_CHUNK {
            return Err(crate::Error::UnknownColumnarTag {
                kind: reader.kind(),
                found: tag,
            });
        }
        let family = ColumnFamily::take(&mut reader)?;
        let seq = reader.take_u64()?;
        let version = Sequence::new(reader.take_u64_descending()?);
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            family,
            seq,
            version,
        })
    }
}

/// The reverse pointer one record's map entry holds: which chunk seqs hold
/// its values, per family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapKey {
    /// The namespace the table belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record the map belongs to.
    pub id: RecordId,
    /// The sequence at which this version was written.
    pub version: Sequence,
}

impl MapKey {
    /// Build a map key.
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

    /// The prefix under which every version of this record's map sorts.
    #[must_use]
    pub fn versions_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: &RecordId,
    ) -> Vec<u8> {
        let mut bytes = RecordKey::versions_prefix(namespace, database, table, id);
        bytes.insert(TABLE_PREFIX_LEN, TAG_MAP);
        bytes
    }
}

impl StoreKey for MapKey {
    type Value = tessari_kv::Value;
    const KIND: KeyKind = KeyKind::Record;

    fn encode(&self) -> Key {
        let mut bytes = Self::versions_prefix(self.namespace, self.database, self.table, &self.id);
        bytes.extend_from_slice(&(!self.version.get()).to_be_bytes());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> crate::Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let tag = reader.take_u8()?;
        if tag != TAG_MAP {
            return Err(crate::Error::UnknownColumnarTag {
                kind: reader.kind(),
                found: tag,
            });
        }
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

/// The pointer naming a family's open chunk: the seq appends land in until
/// the chunk seals (D-SG2-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerKey {
    /// The namespace the table belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// Which column the pointer belongs to.
    pub family: ColumnFamily,
    /// The sequence at which this version was written.
    pub version: Sequence,
}

impl PointerKey {
    /// Build a pointer key.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        family: ColumnFamily,
        version: Sequence,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            family,
            version,
        }
    }

    /// The prefix under which every version of this pointer sorts.
    #[must_use]
    pub fn versions_prefix(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        family: &ColumnFamily,
    ) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(32);
        writer
            .put_u8(KeyKind::Record.tag())
            .put_u32(namespace.get())
            .put_u32(database.get())
            .put_u32(table.get())
            .put_u8(TAG_POINTER);
        family.put(&mut writer);
        writer.finish()
    }
}

impl StoreKey for PointerKey {
    type Value = tessari_kv::Value;
    const KIND: KeyKind = KeyKind::Record;

    fn encode(&self) -> Key {
        let mut bytes =
            Self::versions_prefix(self.namespace, self.database, self.table, &self.family);
        bytes.extend_from_slice(&(!self.version.get()).to_be_bytes());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> crate::Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let tag = reader.take_u8()?;
        if tag != TAG_POINTER {
            return Err(crate::Error::UnknownColumnarTag {
                kind: reader.kind(),
                found: tag,
            });
        }
        let family = ColumnFamily::take(&mut reader)?;
        let version = Sequence::new(reader.take_u64_descending()?);
        reader.finish()?;
        Ok(Self {
            namespace,
            database,
            table,
            family,
            version,
        })
    }
}
