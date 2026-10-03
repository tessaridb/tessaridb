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

/// What a term does in one record: how often it occurs, and how long the record
/// is.
///
/// # Why a posting carries the record's length, which is not a property of the
/// term
///
/// A relevance score needs four numbers. Two describe the collection — how many
/// records there are and how long a typical one is — and are held once, beside
/// the postings, in [`SearchStatistics`]. The other two describe *this* record:
/// how often it holds the term, and how long it is.
///
/// The frequency plainly belongs here. The length is a property of the record,
/// so the tidy place for it would be one entry per record — and it is here
/// instead, repeated once per distinct term. That is deliberate: it makes a
/// score computable from the postings scan **alone**. A scan of one term's
/// postings yields the record, the frequency and the length together, so
/// scoring costs no further read at all, where a separate length entry would
/// cost one point read per candidate — most of what storing the numbers was
/// meant to remove.
///
/// The redundancy also cannot drift. A record's update already deletes its whole
/// posting set and writes a new one, so a changed length rewrites exactly the
/// postings that were being rewritten anyway, by the same code, in the same
/// batch.
///
/// # A posting written before this payload existed
///
/// [`Self::Membership`] is what an older format wrote: the header and nothing
/// after it. It says the term is in the record and no more, which is all
/// `MATCHES` ever needed — so an index written that way keeps answering
/// `MATCHES` correctly and only cannot be **scored**. The distinction is carried
/// by the encoding itself rather than by a declared version, so it cannot
/// disagree with the data it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posting {
    /// The term is in the record. Written before postings carried a payload.
    Membership,
    /// The term is in the record this often, and the record is this long.
    Counted {
        /// Occurrences of this term in this record, **with** repeats.
        frequency: u32,
        /// Tokens in the record's analysed field, **with** repeats.
        ///
        /// The same quantity [`SearchStatistics::terms`] accumulates, so the two
        /// cannot mean different things by "length".
        length: u32,
    },
}

impl StoreValue for Posting {
    fn encode(&self) -> Value {
        let Self::Counted { frequency, length } = *self else {
            // Byte-identical to `NoPayload`, because it is the same statement.
            return Value::from(with_header(0, 0));
        };
        let mut writer = KeyWriter::new();
        writer.put_u32(frequency).put_u32(length);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        if payload.is_empty() {
            return Ok(Self::Membership);
        }
        let mut reader = KeyReader::new(KeyKind::Posting, payload);
        let frequency = reader.take_u32()?;
        let length = reader.take_u32()?;
        // Whatever lists follow are read — and so checked — here too, so a
        // posting that decodes as counted is one whose whole payload is sound.
        take_lists(&mut reader, frequency)?;
        reader.finish()?;
        Ok(Self::Counted { frequency, length })
    }
}

/// The positions flag: the posting lists the term's token ordinals.
const LISTS_POSITIONS: u8 = 1;
/// The offsets flag: the posting lists the term's byte ranges.
const LISTS_OFFSETS: u8 = 2;
/// The fields flag: the posting lists a frequency and a length per member
/// field, after a one-byte field count.
const LISTS_FIELDS: u8 = 4;

/// Where one term sits in one record, as a `POSITIONS` / `OFFSETS` index keeps
/// it (ADR-0100 D4).
///
/// Empty lists are what an index without those options holds, so a reader asks
/// one question whatever the index declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Located {
    /// The term's token ordinals in the record, ascending.
    pub positions: Vec<u32>,
    /// The term's byte ranges in the record's text, start inclusive and end
    /// exclusive, in token order.
    pub offsets: Vec<(u32, u32)>,
    /// For a search member, per field in declaration order: how often the term
    /// occurs in that field and how long that field is (Q-870). Empty for a
    /// field index, and for a member posting written before it was kept.
    ///
    /// What lets a `FROM SEARCH` score BM25F — each field normalised by its own
    /// length — from the postings alone, where without it every candidate's
    /// text had to be read and analysed again.
    pub fields: Vec<(u32, u32)>,
}

impl Posting {
    /// A counted posting with the lists an option asked for.
    ///
    /// The payload is the counted one — frequency and length — then, only when
    /// a list is present, a flags byte naming which follow and the lists
    /// themselves, `frequency` entries each. A posting with neither list is
    /// byte-identical to [`Posting::Counted`]'s encoding, so an index with no
    /// option writes exactly what it always wrote.
    #[must_use]
    pub fn encode_located(frequency: u32, length: u32, located: &Located) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u32(frequency).put_u32(length);
        let mut flags = 0_u8;
        if !located.positions.is_empty() {
            flags |= LISTS_POSITIONS;
        }
        if !located.offsets.is_empty() {
            flags |= LISTS_OFFSETS;
        }
        // A member declares at most a byte's worth of fields; one with more
        // keeps the counted payload, which every reader still scores from text.
        let fields = u8::try_from(located.fields.len())
            .ok()
            .filter(|count| *count > 0);
        if fields.is_some() {
            flags |= LISTS_FIELDS;
        }
        if flags != 0 {
            writer.put_u8(flags);
            for position in &located.positions {
                writer.put_u32(*position);
            }
            for (start, end) in &located.offsets {
                writer.put_u32(*start).put_u32(*end);
            }
            if let Some(count) = fields {
                writer.put_u8(count);
                for (frequency, length) in &located.fields {
                    writer.put_u32(*frequency).put_u32(*length);
                }
            }
        }
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    /// The lists a stored posting carries — empty for one written without them.
    ///
    /// # Errors
    ///
    /// Returns an error for a payload that does not read whole: an unknown flag,
    /// a list shorter or longer than the frequency says.
    pub fn located(bytes: &[u8]) -> Result<Located> {
        let (_, payload) = split_header(bytes, 0)?;
        if payload.is_empty() {
            return Ok(Located::default());
        }
        let mut reader = KeyReader::new(KeyKind::Posting, payload);
        let frequency = reader.take_u32()?;
        reader.take_u32()?;
        let located = take_lists(&mut reader, frequency)?;
        reader.finish()?;
        Ok(located)
    }
}

/// The optional lists after a counted payload, each `frequency` entries long.
fn take_lists(reader: &mut KeyReader<'_>, frequency: u32) -> Result<Located> {
    if reader.remaining() == 0 {
        return Ok(Located::default());
    }
    let flags = reader.take_u8()?;
    if flags & !(LISTS_POSITIONS | LISTS_OFFSETS | LISTS_FIELDS) != 0 || flags == 0 {
        return Err(Error::ReservedFlags { flags });
    }
    let count = usize::try_from(frequency).unwrap_or(usize::MAX);
    let mut located = Located::default();
    if flags & LISTS_POSITIONS != 0 {
        located.positions.reserve(count.min(reader.remaining()));
        for _ in 0..count {
            located.positions.push(reader.take_u32()?);
        }
    }
    if flags & LISTS_OFFSETS != 0 {
        located.offsets.reserve(count.min(reader.remaining()));
        for _ in 0..count {
            let start = reader.take_u32()?;
            located.offsets.push((start, reader.take_u32()?));
        }
    }
    if flags & LISTS_FIELDS != 0 {
        let count = reader.take_u8()?;
        for _ in 0..count {
            let frequency = reader.take_u32()?;
            located.fields.push((frequency, reader.take_u32()?));
        }
    }
    Ok(located)
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
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_types::{DatabaseId, IndexId, NamespaceId, TableId};

    use tessari_types::Value;

    use super::{
        IndexAddress, IndexValues, SearchStatistics, SearchStatisticsKey, StoreKey, StoreValue,
    };

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
    fn a_dictionary_entry_survives_the_round_trip_and_carries_no_record() {
        use super::SearchTermKey;
        let term = IndexValues::of(&[Value::from("vector")]);
        let key = SearchTermKey::new(address(), term.clone());
        let encoded = key.encode();
        let read = SearchTermKey::decode(encoded.as_slice()).expect("a key");
        assert_eq!(read, key);
        // One entry per term, so the key ends where the term ends. A record
        // suffix would make the dictionary as long as the posting list and
        // remove the whole reason for it.
        assert_eq!(
            encoded.as_slice().len(),
            super::INDEX_PREFIX_LEN + term.as_slice().len()
        );
    }

    #[test]
    fn a_member_posting_keeps_each_fields_frequency_and_length() {
        use super::{Located, Posting, StoreValue};
        let located = Located {
            fields: vec![(2, 5), (0, 9), (1, 40)],
            ..Located::default()
        };
        let encoded = Posting::encode_located(3, 54, &located);
        assert_eq!(Posting::located(encoded.as_slice()).unwrap(), located);
        // Still the counted posting every reader knows.
        assert_eq!(
            Posting::decode(encoded.as_slice()).unwrap(),
            Posting::Counted {
                frequency: 3,
                length: 54
            }
        );
        // With no fields it is byte-identical to the counted form.
        assert_eq!(
            Posting::encode_located(3, 54, &Located::default()).as_slice(),
            Posting::Counted {
                frequency: 3,
                length: 54
            }
            .encode()
            .as_slice()
        );
    }

    #[test]
    fn a_surface_pair_and_its_count_survive_the_round_trip_and_a_walk_bounds_them() {
        use super::SearchSurfaceKey;
        let key =
            SearchSurfaceKey::new(address(), "transactions".to_owned(), "transact".to_owned());
        let read = SearchSurfaceKey::decode(key.encode().as_slice()).expect("a key");
        assert_eq!(read, key);
        let counted = SearchSurfaceKey::count(7);
        assert_eq!(
            SearchSurfaceKey::counted(counted.as_slice()).expect("a count"),
            7
        );
        // The leading letters bound exactly the surfaces that begin with them.
        let bounds = SearchSurfaceKey::surface_prefix(&address(), "tr");
        assert!(key.encode().as_slice().starts_with(&bounds));
        let other = SearchSurfaceKey::new(address(), "replicas".to_owned(), "replica".to_owned());
        assert!(!other.encode().as_slice().starts_with(&bounds));
    }

    #[test]
    fn a_dictionary_entry_and_its_postings_spell_the_term_the_same_way() {
        use super::{PostingKey, SearchTermKey};
        // The load-bearing property: a prefix walk of the dictionary finds the
        // terms whose postings a lookup then reads. Two encodings would be two
        // vocabularies, and the walk would reach terms the lookup could not.
        let term = IndexValues::of(&[Value::from("vector")]);
        let dictionary = SearchTermKey::new(address(), term.clone()).encode();
        let postings = PostingKey::term_prefix(&address(), &term);
        assert_eq!(
            &dictionary.as_slice()[super::INDEX_PREFIX_LEN..],
            &postings[super::INDEX_PREFIX_LEN..]
        );
        // And only the kind byte differs, so scanning one never reaches the
        // other.
        assert_ne!(dictionary.as_slice()[0], postings[0]);
    }

    #[test]
    fn a_term_prefix_bounds_exactly_the_terms_that_begin_with_it() {
        use super::SearchTermKey;
        let bounds = SearchTermKey::term_prefix(&address(), "vect");
        let under = ["vect", "vector", "vectorised"];
        for term in under {
            let key = SearchTermKey::new(address(), IndexValues::of(&[Value::from(term)])).encode();
            assert!(key.as_slice().starts_with(&bounds), "{term} not under vect");
        }
        // And nothing else is, including the words a naive substring match
        // would admit — the escape is byte-local, so no filtering step is owed.
        for term in ["vec", "invective", "wave"] {
            let key = SearchTermKey::new(address(), IndexValues::of(&[Value::from(term)])).encode();
            assert!(!key.as_slice().starts_with(&bounds), "{term} under vect");
        }
    }

    #[test]
    fn a_term_frequency_survives_the_round_trip() {
        use super::TermStatistics;
        for documents in [0_u64, 1, 12_345, u64::MAX] {
            let held = TermStatistics::new(documents);
            let read = TermStatistics::decode(held.encode().as_slice()).expect("statistics");
            assert_eq!(read, held, "{documents}");
        }
    }

    #[test]
    fn a_dictionary_entry_from_a_later_format_is_refused_rather_than_half_read() {
        use super::TermStatistics;
        // `max_impact` will arrive as a longer payload. Reading such an entry as
        // though it were this build's would report a frequency out of a format
        // whose meaning this build cannot check — the same refusal a posting
        // with a trailing byte already gets.
        let mut bytes = TermStatistics::new(7).encode().as_slice().to_vec();
        bytes.push(0);
        assert!(
            TermStatistics::decode(&bytes).is_err(),
            "trailing byte read"
        );
    }

    #[test]
    fn a_counted_posting_survives_the_round_trip() {
        use super::Posting;
        for (frequency, length) in [(1_u32, 1_u32), (3, 97), (u32::MAX, u32::MAX), (1, u32::MAX)] {
            let held = Posting::Counted { frequency, length };
            let read = Posting::decode(held.encode().as_slice()).expect("a posting");
            assert_eq!(read, held, "{frequency}/{length}");
        }
    }

    #[test]
    fn a_posting_with_no_payload_is_the_membership_one_an_older_format_wrote() {
        use super::{NoPayload, Posting};
        // Byte-identical, because it is the same statement — which is what lets
        // an index written before postings carried a payload keep answering
        // `MATCHES` instead of failing to decode.
        assert_eq!(
            Posting::Membership.encode().as_slice(),
            NoPayload.encode().as_slice()
        );
        let read = Posting::decode(NoPayload.encode().as_slice()).expect("a posting");
        assert_eq!(read, Posting::Membership);
    }

    #[test]
    fn a_counted_posting_is_not_mistaken_for_a_membership_one() {
        use super::Posting;
        // The distinction is carried by the encoding rather than by a declared
        // version, so it cannot disagree with the data. A zero frequency is
        // still `Counted`: it is a statement, where `Membership` is the absence
        // of one.
        let zero = Posting::Counted {
            frequency: 0,
            length: 0,
        };
        assert_ne!(
            zero.encode().as_slice(),
            Posting::Membership.encode().as_slice()
        );
        assert_eq!(
            Posting::decode(zero.encode().as_slice()).expect("a posting"),
            zero
        );
    }

    #[test]
    fn a_truncated_posting_payload_is_refused_rather_than_read_short() {
        use super::Posting;
        let full = Posting::Counted {
            frequency: 7,
            length: 11,
        };
        let encoded = full.encode();
        let bytes = encoded.as_slice();
        // Every cut between the header and the end: a decoder that read a short
        // payload as a smaller number would return a plausible wrong score.
        for cut in 3..bytes.len() {
            assert!(
                Posting::decode(&bytes[..cut]).is_err(),
                "{cut} bytes decoded when it should not"
            );
        }
    }

    #[test]
    fn a_posting_payload_longer_than_the_format_is_refused() {
        use super::Posting;
        // Found by falsification: dropping `reader.finish()` left every other
        // assertion green, because a short payload is caught by the reads
        // themselves and nothing here asked about a long one. Trailing bytes
        // mean the value was written by something this build does not
        // understand, and reading the prefix of it would be reading two numbers
        // out of a structure that has more.
        let mut bytes = Posting::Counted {
            frequency: 7,
            length: 11,
        }
        .encode()
        .as_slice()
        .to_vec();
        bytes.push(0);
        assert!(Posting::decode(&bytes).is_err(), "trailing byte accepted");
    }

    #[test]
    fn a_vector_node_survives_the_round_trip() {
        use super::{QuantizedVector, RecordId, StoredVector, VectorNode, VectorNodeKey};
        let components = vec![0.123, 0.999, 0.0, 1.0, 0.5];
        let node = VectorNode::new(
            StoredVector::Full(components.clone()),
            vec![RecordId::Int(1), RecordId::Int(2)],
        );
        let encoded = node.encode();
        let read = VectorNode::decode(encoded.as_slice()).expect("a node");
        assert_eq!(read, node);

        let coded = QuantizedVector::of(&components).expect("codes");
        let quantized = VectorNode::new(
            StoredVector::Quantized(coded),
            vec![RecordId::Int(1), RecordId::Int(2)],
        );
        let small = quantized.encode();
        assert_eq!(
            VectorNode::decode(small.as_slice()).expect("a node"),
            quantized
        );
        // The layout `stored_bytes` states: the two nodes share their neighbour
        // lists, so their sizes differ by exactly the two vectors' forms.
        assert_eq!(
            encoded.as_slice().len() - small.as_slice().len(),
            node.vector.stored_bytes() - quantized.vector.stored_bytes()
        );

        let key = VectorNodeKey::new(address(), 0, RecordId::Int(7));
        let bytes = key.encode();
        assert_eq!(VectorNodeKey::decode(bytes.as_slice()).expect("a key"), key);
    }

    #[test]
    fn a_full_precision_node_keeps_the_bytes_it_always_had() {
        // Every vector index written before `QUANTIZED` holds these bytes, and
        // a node is never rewritten by an upgrade — so the full form's encoding
        // is pinned, not merely round-tripped.
        use super::{RecordId, StoredVector, VectorNode};
        let node = VectorNode::new(StoredVector::Full(vec![0.5, -1.0]), vec![RecordId::Int(3)]);
        assert_eq!(node.encode().as_slice(), GOLDEN_FULL_NODE);
    }

    const GOLDEN_FULL_NODE: &[u8] = &[
        1, 0, 0, 0, 0, 2, 63, 224, 0, 0, 0, 0, 0, 0, 191, 240, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1,
        128, 0, 0, 0, 0, 0, 0, 3,
    ];

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

    #[test]
    fn the_leading_values_of_a_composite_entry_are_the_bytes_a_shorter_entry_encodes() {
        // What an order over the leading field of a composite index compares.
        // Whether two entries share a `last` is asked of the *bytes*, because
        // the encoding cannot be reversed — so the bytes have to be exactly what
        // a one-value entry encodes, or a tie group would be recognised by a
        // rule the writer does not follow.
        let composite = IndexValues::of(&[Value::from("ward"), Value::from("ada")]);
        let other_first = IndexValues::of(&[Value::from("ward"), Value::from("zoe")]);
        let other_last = IndexValues::of(&[Value::from("wardle"), Value::from("ada")]);

        let one = composite.leading_of(1).unwrap();
        assert_eq!(one, IndexValues::leading(&[Value::from("ward")]).as_slice());
        assert_eq!(one, other_first.leading_of(1).unwrap());
        assert_ne!(one, other_last.leading_of(1).unwrap());

        // `ward` must not be the leading run of `wardle`, or the tie group would
        // swallow the next value's entries. This is the self-delimiting property
        // stated as a test rather than as a comment.
        assert!(!other_last.leading_of(1).unwrap().starts_with(one));
    }

    #[test]
    fn asking_for_more_fields_than_the_entry_holds_compares_all_of_them() {
        // The failure this refuses is silent: comparing *fewer* values than
        // asked for would merge tie groups that are not tied, and the answer
        // would be short with every record it returned real.
        let entry = IndexValues::of(&[Value::from("ward"), Value::from("ada")]);
        let all = entry.leading_of(2).unwrap();
        assert_eq!(all, entry.leading_of(9).unwrap());
        assert_eq!(
            all,
            IndexValues::leading(&[Value::from("ward"), Value::from("ada")]).as_slice()
        );
        assert_eq!(entry.leading_of(0).unwrap(), b"");
    }

    #[test]
    fn two_spellings_of_one_number_lead_with_the_same_bytes() {
        // The normalisation that makes the encoding one-way is what makes byte
        // equality the *right* tie test: `1` and `1.0` are one value, so they
        // belong in one tie group and must not be walked as two.
        let integer = IndexValues::of(&[Value::from(1_i64), Value::from("a")]);
        let float = IndexValues::of(&[Value::from(1.0_f64), Value::from("b")]);
        assert_eq!(integer.leading_of(1).unwrap(), float.leading_of(1).unwrap());
    }

    /// Every shape a posting's lists take, written and read back, and the plain
    /// counted posting unchanged by the option machinery (G051 T7.6).
    #[test]
    fn a_posting_carries_its_lists_and_reads_them_back() {
        use super::{Located, Posting};
        let plain = Posting::Counted {
            frequency: 2,
            length: 9,
        }
        .encode();
        assert_eq!(
            Posting::encode_located(2, 9, &Located::default()),
            plain,
            "an index with no option writes exactly what it always wrote"
        );
        for located in [
            Located {
                positions: vec![1, 7],
                ..Located::default()
            },
            Located {
                offsets: vec![(0, 3), (40, 44)],
                ..Located::default()
            },
            Located {
                positions: vec![1, 7],
                offsets: vec![(0, 3), (40, 44)],
                ..Located::default()
            },
        ] {
            let stored = Posting::encode_located(2, 9, &located);
            assert_eq!(Posting::located(stored.as_slice()).unwrap(), located);
            assert_eq!(
                Posting::decode(stored.as_slice()).unwrap(),
                Posting::Counted {
                    frequency: 2,
                    length: 9
                }
            );
        }
        assert_eq!(
            Posting::located(plain.as_slice()).unwrap(),
            Located::default()
        );
    }

    #[test]
    fn a_posting_whose_lists_do_not_add_up_is_refused() {
        use super::{Located, Posting};
        let stored = Posting::encode_located(
            2,
            9,
            &Located {
                positions: vec![1, 7],
                ..Located::default()
            },
        );
        let bytes = stored.as_slice();
        // One position short.
        let short = &bytes[..bytes.len() - 4];
        assert!(Posting::located(short).is_err());
        assert!(Posting::decode(short).is_err());
        // One byte too many.
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(Posting::located(&long).is_err());
        // A flag this build does not know (4 names the fields list).
        let mut unknown = bytes.to_vec();
        let flags_at = unknown.len() - 9;
        unknown[flags_at] |= 8;
        assert!(matches!(
            Posting::located(&unknown),
            Err(crate::Error::ReservedFlags { .. })
        ));
    }
}
