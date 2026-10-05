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

mod containment;
mod posting;
mod quantized;
mod search;
mod statistics;
mod vectors;
use tessari_kv::{Key, Value};
use tessari_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

use crate::error::{Error, Result};
use crate::index_value;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
use crate::value::{StoreValue, split_header, with_header};
pub use containment::ContainmentKey;
use posting::take_values;
pub use posting::{Located, Posting};
pub use quantized::{QuantizedVector, StoredVector};
pub use search::{
    PostingKey, SearchStatistics, SearchStatisticsKey, SearchSuffixKey, SearchSurfaceKey,
    SearchTermKey, TermStatistics, UniqueIndexKey,
};
pub use statistics::{IndexChanges, IndexChangesKey, IndexStatistics, IndexStatisticsKey};
pub use vectors::{
    SpatialRefinement, SpatialRefinementKey, VectorNode, VectorNodeKey, VectorRecall,
    VectorRecallKey,
};

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

    pub(crate) fn read(reader: &mut KeyReader<'_>) -> Result<Self> {
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
    pub fn of(values: &[tessari_types::Value]) -> Self {
        let mut writer = KeyWriter::new();
        for value in values {
            index_value::put(&mut writer, value);
        }
        writer.put_u8(index_value::END);
        Self(writer.finish())
    }

    /// The encoding of the first *k* indexed values, with **no** terminator.
    ///
    /// What a **prefix** of an entry's values encodes to, and what makes a
    /// composite index readable at all: the entries for one `last` are
    /// contiguous, but a complete [`IndexValues`] ends with a marker that a
    /// longer key does not have in that position, so the complete form of one
    /// value is not a byte-prefix of a two-value key.
    ///
    /// It is exact rather than approximate, and for a stated reason: every
    /// value's encoding is **self-delimiting** — a variable-length one ends with
    /// an escape and a terminator, and the rest are fixed width — so these bytes
    /// are a byte-prefix of a key exactly when that key's first *k* values are
    /// these. `enc("ab")` is therefore not a prefix of `enc("abc")`, which is
    /// what a scan over "every entry whose first value is `ab`" depends on.
    ///
    /// For a single value on a single-field index it is the complete form minus
    /// its marker, and the key it must match still begins with it — which is why
    /// there is one rule here rather than a partial path beside a complete one.
    #[must_use]
    pub fn leading(values: &[tessari_types::Value]) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        for value in values {
            index_value::put(&mut writer, value);
        }
        writer.finish()
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

    /// The text, when these are exactly one string.
    ///
    /// The single exception to this type's opacity, and it is narrow on purpose.
    /// What makes the encoding irreversible is **number** normalisation — `1`,
    /// `1.0` and decimal `1.00` become the same bytes, so no reader can say
    /// which was written. A string is written as its own bytes under a
    /// byte-local escape and comes back exactly.
    ///
    /// It exists for the term dictionary, where the stored key *is* the word and
    /// a caller walking it needs the word: to measure an edit distance against
    /// it, to offer it as a completion, or to name it in a refusal. `None` for
    /// anything that is not a lone string, so an ordered index's entry cannot be
    /// read back as a term.
    #[must_use]
    pub fn as_text(&self) -> Option<String> {
        index_value::lone_string(&self.0)
    }

    /// The bytes of this entry's first `fields` values, without the terminator.
    ///
    /// **What a tie group is, when the order names fewer fields than the index
    /// has.** An entry of `(last, first)` is ordered by `last`, then `first`,
    /// then the record's identity, so the entries sharing one `last` are a
    /// contiguous run — and `ORDER BY last` has to know where that run ends
    /// before it may cut at a bound, or it takes the wrong members of it.
    ///
    /// The values are **not decoded**, and they do not need to be. Two entries
    /// agree on their first `fields` values exactly when these bytes are equal,
    /// because every value's encoding is self-delimiting (the same property
    /// [`IndexValues::leading`] rests on). That the encoding cannot be reversed
    /// is therefore not an obstacle here: the question is agreement, not
    /// identity. And the normalisation that destroys reversibility is what makes
    /// byte equality the *right* test rather than a workaround — `1` and `1.0`
    /// are one value, so they belong in one tie group, and the bytes say so.
    ///
    /// Returns the whole encoding minus its terminator when `fields` is at least
    /// the number of values held, so a caller asking for more fields than the
    /// index has compares everything rather than silently comparing less.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are truncated or carry an unknown tag,
    /// which for an entry this store wrote is unreachable.
    pub fn leading_of(&self, fields: usize) -> Result<&[u8]> {
        let mut reader = KeyReader::new(KeyKind::SecondaryIndex, &self.0);
        let start = reader.position();
        let mut seen = 0;
        while seen < fields && reader.peek()? != index_value::END {
            index_value::skip(&mut reader)?;
            seen = seen.saturating_add(1);
        }
        Ok(reader.consumed_since(start))
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

#[cfg(test)]
mod tests;
