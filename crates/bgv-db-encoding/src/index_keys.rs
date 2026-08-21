//! Index entry keys.
//!
//! Two kinds, and the difference between them is one suffix.
//!
//! ```text
//! secondary  <0x10> <ns:u32> <db:u32> <tb:u32> <ix:u32> <values…> <0x00> <record-id>
//! unique     <0x11> <ns:u32> <db:u32> <tb:u32> <ix:u32> <values…> <0x00>
//! ```
//!
//! A unique entry carries **no record id**, which is what enforces uniqueness:
//! two records with the same indexed value produce the same key, so the second
//! write collides with the first instead of sitting beside it. Uniqueness is
//! therefore a property of the key layout rather than a check somebody has to
//! remember to run.
//!
//! The two kinds could have been one type with an optional suffix. They are not,
//! because a decoder would then have to guess whether trailing bytes are a
//! record id or the start of nothing — and a key grammar whose parse depends on
//! a guess is the class of mistake this whole layer exists to make impossible.
//!
//! The field list ends with a terminator even though an index's arity is fixed
//! by its definition. That keeps a key decodable on its own, without the catalog
//! entry that describes it, which matters exactly when something has gone wrong
//! and an operator is looking at bytes.

use bgv_db_kv::{Key, Value};
use bgv_db_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

use crate::error::Result;
use crate::index_value;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};

/// Bytes of an index key that identify one index: the kind tag, the three
/// tenancy identifiers, and the index id.
pub const INDEX_PREFIX_LEN: usize = 17;

/// Which index, on which table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexAddress {
    /// The namespace.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The table the index is on.
    pub table: TableId,
    /// The index itself.
    pub index: IndexId,
}

impl IndexAddress {
    /// Name one index.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        index: IndexId,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            index,
        }
    }

    /// The prefix every entry of this index shares.
    ///
    /// Always [`INDEX_PREFIX_LEN`] bytes long, which is what lets a prefix
    /// filter extract it.
    #[must_use]
    pub fn prefix(&self, kind: KeyKind) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(INDEX_PREFIX_LEN);
        writer
            .put_u8(kind.tag())
            .put_u32(self.namespace.get())
            .put_u32(self.database.get())
            .put_u32(self.table.get())
            .put_u32(self.index.get());
        writer.finish()
    }

    fn read(reader: &mut KeyReader<'_>) -> Result<Self> {
        Ok(Self {
            namespace: NamespaceId::new(reader.take_u32()?),
            database: DatabaseId::new(reader.take_u32()?),
            table: TableId::new(reader.take_u32()?),
            index: IndexId::new(reader.take_u32()?),
        })
    }
}

/// The indexed field values, in their order-preserving form.
///
/// Opaque on purpose. The encoding normalises numbers — `1`, `1.0` and decimal
/// `1.00` become the same bytes — so it cannot be reversed, and a type that
/// pretended otherwise would invite a caller to read a value back out of an
/// index and get a different spelling than the record holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexValues(Vec<u8>);

impl IndexValues {
    /// Encode the values of one entry, in the index's declared field order.
    #[must_use]
    pub fn of(values: &[bgv_db_types::Value]) -> Self {
        let mut writer = KeyWriter::new();
        for value in values {
            index_value::put(&mut writer, value);
        }
        writer.put_u8(index_value::END);
        Self(writer.finish())
    }

    /// The bytes shared by every entry whose first indexed value is a string
    /// beginning with `prefix`.
    ///
    /// Not an [`IndexValues`] — it is deliberately *not* a complete encoding,
    /// because a complete one selects one value and this selects a range. Append
    /// it to an index's own prefix and scan.
    #[must_use]
    pub fn string_prefix(prefix: &str) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        index_value::put_string_prefix(&mut writer, prefix);
        writer.finish()
    }

    /// The encoded bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

/// One entry of a non-unique index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryIndexKey {
    /// Which index this entry belongs to.
    pub address: IndexAddress,
    /// The indexed values.
    pub values: IndexValues,
    /// The record the entry points at.
    pub id: RecordId,
}

/// One entry of a unique index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueIndexKey {
    /// Which index this entry belongs to.
    pub address: IndexAddress,
    /// The indexed values.
    pub values: IndexValues,
}

impl SecondaryIndexKey {
    /// Build an entry key.
    #[must_use]
    pub const fn new(address: IndexAddress, values: IndexValues, id: RecordId) -> Self {
        Self {
            address,
            values,
            id,
        }
    }

    /// The prefix shared by every entry holding these values.
    ///
    /// Bounding a scan with this is what turns "find the records with this
    /// value" into a range read.
    #[must_use]
    pub fn values_prefix(address: &IndexAddress, values: &IndexValues) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::SecondaryIndex);
        bytes.extend_from_slice(values.as_slice());
        bytes
    }
}

impl UniqueIndexKey {
    /// Build an entry key.
    #[must_use]
    pub const fn new(address: IndexAddress, values: IndexValues) -> Self {
        Self { address, values }
    }
}

impl StoreKey for SecondaryIndexKey {
    type Value = NoPayload;

    const KIND: KeyKind = KeyKind::SecondaryIndex;

    fn encode(&self) -> Key {
        let mut bytes = Self::values_prefix(&self.address, &self.values);
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

impl StoreKey for UniqueIndexKey {
    type Value = IndexTarget;

    const KIND: KeyKind = KeyKind::UniqueIndex;

    fn encode(&self) -> Key {
        let mut bytes = self.address.prefix(Self::KIND);
        bytes.extend_from_slice(self.values.as_slice());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let values = take_values(&mut reader)?;
        reader.finish()?;
        Ok(Self { address, values })
    }
}

/// The record a unique entry points at.
///
/// A unique key cannot carry the record id — that is what makes it unique — so
/// the value carries it instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTarget {
    /// The record's identity within the table.
    pub id: RecordId,
}

impl IndexTarget {
    /// Point at a record.
    #[must_use]
    pub const fn new(id: RecordId) -> Self {
        Self { id }
    }
}

impl StoreValue for IndexTarget {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::UniqueIndex, payload);
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self { id })
    }
}

/// The value of an entry that says everything in its key.
///
/// A non-unique entry already carries its record id in the key, so a value
/// repeating it would be the same fact written twice — and two statements of one
/// fact can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoPayload;

impl StoreValue for NoPayload {
    fn encode(&self) -> Value {
        Value::from(with_header(0, 0))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        if payload.is_empty() {
            return Ok(Self);
        }
        Err(crate::error::Error::TombstoneWithPayload { len: payload.len() })
    }
}

/// Walk the field list and keep its bytes verbatim.
///
/// The fields are not decoded — the encoding normalises numbers and so cannot be
/// reversed — but the walk still has to be exact, because whatever follows the
/// list starts where the walk stops.
fn take_values(reader: &mut KeyReader<'_>) -> Result<IndexValues> {
    let start = reader.position();
    while reader.peek()? != index_value::END {
        index_value::skip(reader)?;
    }
    reader.take_u8()?;
    Ok(IndexValues(reader.consumed_since(start).to_vec()))
}
