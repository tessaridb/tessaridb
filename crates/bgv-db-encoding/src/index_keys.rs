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

/// One posting of a search index: this term, in this record.
///
/// Structurally an entry of a secondary index — an address, a value and a
/// record — and a **different key kind** all the same. Two reasons: the scan
/// patterns differ (a term lookup is a prefix read where an ordered index is
/// also read as a range between two values), and a keyspace that can be swept
/// on its own is a keyspace that can be reclaimed on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingKey {
    /// Which index this posting belongs to.
    pub address: IndexAddress,
    /// The term, order-encoded the way every indexed value is.
    pub term: IndexValues,
    /// The record holding it.
    pub id: RecordId,
}

/// What one search index knows about its collection as a whole.
///
/// A posting says a term is in a document. A **score** says how much that
/// matters, and that cannot be read off one document: it needs the size of the
/// collection and the length of a typical member of it. Neither is a property of
/// any record, so neither can be recomputed from one — they are maintained,
/// beside the postings they summarise and in the same batch.
///
/// The key is exactly an index prefix with no suffix, so one index has exactly
/// one of these and finding it is a point read rather than a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchStatisticsKey {
    /// Which index these statistics describe.
    pub address: IndexAddress,
}

impl SearchStatisticsKey {
    /// Name the statistics of one index.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for SearchStatisticsKey {
    type Value = SearchStatistics;

    const KIND: KeyKind = KeyKind::SearchStatistics;

    fn encode(&self) -> Key {
        Key::from(self.address.prefix(Self::KIND))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { address })
    }
}

/// The two numbers a ranking is measured against.
///
/// `documents` counts the records that contribute at least one term. A record
/// whose indexed field is absent, empty, or not text is not in the index and is
/// not counted — the same "not in this index at all" answer the postings give.
///
/// `terms` is the **token** count with repeats, not the number of distinct
/// terms, because it exists to divide by `documents` and yield an average
/// document *length*. The postings deduplicate and this does not; both are
/// computed from one analyzer pass over the same text, so the two cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchStatistics {
    /// How many records hold at least one term.
    pub documents: u64,
    /// How many tokens those records hold in total.
    pub terms: u64,
}

impl SearchStatistics {
    /// State both numbers.
    #[must_use]
    pub const fn new(documents: u64, terms: u64) -> Self {
        Self { documents, terms }
    }

    /// The length of a typical document, or `None` when there are none.
    ///
    /// An empty index has no average, and answering zero would divide a score by
    /// it. The absence is returned so the caller decides what an unmeasurable
    /// collection means, rather than being handed a number that is not one.
    #[must_use]
    pub fn average_length(self) -> Option<f64> {
        if self.documents == 0 {
            return None;
        }
        let documents = approximate(self.documents);
        let terms = approximate(self.terms);
        Some(terms / documents)
    }
}

/// A count as a float, without an `as` cast.
///
/// `f64` has no `From<u64>` because the conversion loses precision past 2^53,
/// and an `as` cast would perform it silently — which is exactly the class of
/// truncation this project refuses to write. Splitting the value into two halves
/// that *do* convert exactly reaches the same number the cast would, by an
/// arithmetic that says what it is doing.
fn approximate(count: u64) -> f64 {
    /// One more than the largest `u32`, as a float.
    const SHIFT: f64 = 4_294_967_296.0;
    let high = u32::try_from(count >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(count & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    f64::from(high).mul_add(SHIFT, f64::from(low))
}

impl StoreValue for SearchStatistics {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u64(self.documents).put_u64(self.terms);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SearchStatistics, payload);
        let documents = reader.take_u64()?;
        let terms = reader.take_u64()?;
        reader.finish()?;
        Ok(Self { documents, terms })
    }
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

impl PostingKey {
    /// Build a posting key.
    #[must_use]
    pub const fn new(address: IndexAddress, term: IndexValues, id: RecordId) -> Self {
        Self { address, term, id }
    }

    /// The prefix shared by every posting of one term.
    ///
    /// Bounding a scan with this is what turns "find the records holding this
    /// word" into a range read.
    #[must_use]
    pub fn term_prefix(address: &IndexAddress, term: &IndexValues) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::Posting);
        bytes.extend_from_slice(term.as_slice());
        bytes
    }
}

impl StoreKey for PostingKey {
    type Value = NoPayload;

    const KIND: KeyKind = KeyKind::Posting;

    fn encode(&self) -> Key {
        let mut bytes = Self::term_prefix(&self.address, &self.term);
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let term = take_values(&mut reader)?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self { address, term, id })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_types::{DatabaseId, IndexId, NamespaceId, TableId};

    use super::{IndexAddress, SearchStatistics, SearchStatisticsKey, StoreKey, StoreValue};

    fn address() -> IndexAddress {
        IndexAddress::new(
            NamespaceId::new(3),
            DatabaseId::new(4),
            TableId::new(5),
            IndexId::new(6),
        )
    }

    #[test]
    fn one_index_has_exactly_one_statistics_key() {
        // No suffix, so the key *is* the index prefix — which is what makes
        // reading it a point read rather than a scan for the one entry.
        let key = SearchStatisticsKey::new(address());
        let encoded = key.encode();
        assert_eq!(encoded.as_slice().len(), super::INDEX_PREFIX_LEN);
        let read = SearchStatisticsKey::decode(encoded.as_slice()).expect("a key");
        assert_eq!(read, key);
    }

    #[test]
    fn both_counts_survive_the_round_trip() {
        let held = SearchStatistics::new(1_234, 98_765);
        let encoded = held.encode();
        let read = SearchStatistics::decode(encoded.as_slice()).expect("statistics");
        assert_eq!(read, held);
    }

    #[test]
    fn an_empty_index_has_no_average_length() {
        // Not zero: a score divides by this, and dividing by a number that is
        // not one is worse than being told there is no number.
        assert_eq!(SearchStatistics::default().average_length(), None);
        assert_eq!(SearchStatistics::new(0, 0).average_length(), None);
    }

    #[test]
    fn the_average_is_tokens_over_documents() {
        let held = SearchStatistics::new(4, 30);
        assert_eq!(held.average_length(), Some(7.5));
    }

    #[test]
    fn a_count_converts_without_a_cast_and_without_saturating() {
        // The split-halves conversion has to reach the same number a cast would,
        // including past `u32`, or a large collection would be ranked against a
        // length that is not its own.
        let held = SearchStatistics::new(1, u64::from(u32::MAX) + 1);
        assert_eq!(held.average_length(), Some(4_294_967_296.0));
        let bigger = SearchStatistics::new(2, 1 << 40);
        assert_eq!(bigger.average_length(), Some(549_755_813_888.0));
    }
}
