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

use tessari_kv::{Key, Value};
use tessari_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

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

/// The recall one vector index was last measured at.
///
/// A vector index answers **approximately**, so the only number that says
/// whether its answers are worth having is the fraction of the true nearest it
/// actually returns. That is a property of a measurement and not of a
/// declaration — a figure derived from the build parameters would be a number
/// nobody checked wearing the name of one somebody did.
///
/// Beside the index rather than on its definition, for the reason
/// [`SearchStatisticsKey`] is: this is a measurement derived from the log, and
/// the definition is what the language wrote. The key is **derived from the
/// [`IndexAddress`]** rather than stored beside it, so it cannot come to name
/// the wrong index, and it is cleared as part of the index's keyspace when the
/// entries are — which is what stops a rebuild leaving a figure describing a
/// graph that no longer exists.
///
/// The key is exactly an index prefix with no suffix, so one index has exactly
/// one of these and finding it is a point read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorRecallKey {
    /// Which index this measurement describes.
    pub address: IndexAddress,
}

impl VectorRecallKey {
    /// Name the measurement of one index.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for VectorRecallKey {
    type Value = VectorRecall;

    const KIND: KeyKind = KeyKind::VectorRecall;

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

/// A measured recall, and everything needed to read it.
///
/// # Why a bare percentage is not stored
///
/// Recall decays as records are added after the measurement — the graph keeps
/// answering, and answers less of the truth — so a lone figure describes a store
/// that may no longer exist, and it goes stale in silence. Every field here
/// exists so a reader can tell whether the number still means anything:
///
/// - `at` — recall@10 and recall@1 are different numbers.
/// - `sample` — a figure from four queries is not a figure from four hundred.
/// - `records` — how large the store was when it was measured, so growth since
///   is visible rather than hidden.
/// - `neighbours` and `exploration` — the engine constants in force; a recall
///   measured at one budget does not describe another.
///
/// Absence of this value means **never measured**, which is a different
/// statement from a measured zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorRecall {
    /// The fraction of the true nearest the walk returned, as a percentage.
    pub recall: u32,
    /// How many neighbours each query asked for.
    pub at: u32,
    /// How many queries the figure is an average over.
    pub sample: u32,
    /// How many records the index held when it was measured.
    pub records: u64,
    /// The neighbour count each node was built with.
    pub neighbours: u32,
    /// The exploration budget the measuring walks spent.
    pub exploration: u32,
}

impl StoreValue for VectorRecall {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u32(self.recall)
            .put_u32(self.at)
            .put_u32(self.sample)
            .put_u64(self.records)
            .put_u32(self.neighbours)
            .put_u32(self.exploration);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::VectorRecall, payload);
        let recall = reader.take_u32()?;
        let at = reader.take_u32()?;
        let sample = reader.take_u32()?;
        let records = reader.take_u64()?;
        let neighbours = reader.take_u32()?;
        let exploration = reader.take_u32()?;
        reader.finish()?;
        Ok(Self {
            recall,
            at,
            sample,
            records,
            neighbours,
            exploration,
        })
    }
}

/// What refining one spatial index's candidates last cost.
///
/// A spatial index answers by **bounding box**, and a box is not a geometry — so
/// every read is filter-and-refine, and the number that says whether the filter
/// is earning its keep is how many records it offers against how many survive.
/// A ratio near one means the stored boxes approximate their geometries well; a
/// large one means the index is doing work the exact predicate throws away.
/// Without it a query budget is tuned by intuition, and a structurally bad row —
/// a river, a road, a border, whose box is many times its own area — is
/// invisible.
///
/// Beside the index rather than on its definition, for the reason
/// [`VectorRecallKey`] is: this is a measurement derived from the log, and the
/// definition is what the language wrote. The key is **derived from the
/// [`IndexAddress`]**, so it cannot come to name the wrong index, and it is
/// cleared as part of the index's keyspace when the entries are — which is what
/// stops a rebuild leaving a figure describing a covering that no longer exists.
///
/// The key is exactly an index prefix with no suffix, so one index has exactly
/// one of these and finding it is a point read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialRefinementKey {
    /// Which index this measurement describes.
    pub address: IndexAddress,
}

impl SpatialRefinementKey {
    /// Name the measurement of one index.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }
}

impl StoreKey for SpatialRefinementKey {
    type Value = SpatialRefinement;

    const KIND: KeyKind = KeyKind::SpatialRefinement;

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

/// The counts a covering is judged by, and everything needed to read them.
///
/// # Why counts rather than a ratio
///
/// There are **two** ratios here and they name two different repairs, so a
/// single stored number would answer neither question:
///
/// - `reached` over `admitted` is how loose the stored boxes are. The cells
///   offer records the box test then throws away.
/// - `entries` over `reached` is how fragmented the covering is. One record with
///   an awkward shape occupies many cells, and every one of them is an entry the
///   traversal reads to arrive at the same record — which is exactly the river,
///   the road and the border the measurement exists to make visible.
///
/// Storing the counts leaves both derivable and neither asserted.
///
/// # Why it cannot go stale in silence
///
/// The counts describe the store as it was when the index was built. `sample`
/// says how many queries stand behind them — a figure from four is not a figure
/// from four hundred — and `records` says how large the index was, so growth
/// since is visible rather than hidden.
///
/// # The query record is not counted
///
/// Each measuring query is a record's own box, and a record always reaches
/// itself and always survives its own box test. Counting it would add one to
/// both sides of every ratio and pull each one toward one, which is to say
/// toward "healthy" — so the record being asked about is excluded from both.
/// A store whose records never reach one another therefore measures nothing at
/// all rather than measuring a perfect score.
///
/// Absence of this value means **never measured**, which is a different
/// statement from a measured zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialRefinement {
    /// Index entries the measuring reads walked.
    pub entries: u64,
    /// Distinct records those entries named.
    pub reached: u64,
    /// How many of those the box test admitted.
    pub admitted: u64,
    /// How many queries the counts are a total over.
    pub sample: u32,
    /// How many records the index held when it was measured.
    pub records: u64,
}

impl SpatialRefinement {
    /// Records offered per record kept, as a percentage.
    ///
    /// `None` when nothing was admitted — not because there was no measurement,
    /// but because the ratio is unbounded there. A covering that offered records
    /// and kept none is the worst refinement there is, and the counts beside this
    /// say so plainly; collapsing it to a number would either invent a ceiling or
    /// print `0`, which reads as a perfect filter.
    ///
    /// `checked_div` rather than a guard and a `saturating_div`: the absence and
    /// the division are the same fact, so writing them as two would let a later
    /// edit separate them. `saturating_div` also does not saturate a zero
    /// divisor — it panics — which is a poor thing to reach for in a database.
    #[must_use]
    pub const fn refinement(self) -> Option<u64> {
        self.reached.saturating_mul(100).checked_div(self.admitted)
    }

    /// Entries read per record reached, as a percentage.
    ///
    /// `None` for the same reason [`Self::refinement`] returns one.
    #[must_use]
    pub const fn fragmentation(self) -> Option<u64> {
        self.entries.saturating_mul(100).checked_div(self.reached)
    }
}

impl StoreValue for SpatialRefinement {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer
            .put_u64(self.entries)
            .put_u64(self.reached)
            .put_u64(self.admitted)
            .put_u32(self.sample)
            .put_u64(self.records);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SpatialRefinement, payload);
        let entries = reader.take_u64()?;
        let reached = reader.take_u64()?;
        let admitted = reader.take_u64()?;
        let sample = reader.take_u32()?;
        let records = reader.take_u64()?;
        reader.finish()?;
        Ok(Self {
            entries,
            reached,
            admitted,
            sample,
            records,
        })
    }
}

/// One record's place in a vector index's graph.
///
/// ```text
/// key    <0x13> <ns:u32> <db:u32> <tb:u32> <ix:u32> <level:u8> <record-id>
/// value  <dimensions:u32> <component:f64 × dimensions> <neighbours:u32> <record-id × neighbours>
/// ```
///
/// # The level byte is reserved and always zero
///
/// A hierarchical graph assigns each node a level, and the layers improve
/// routing at large collection sizes. This index has one layer, because a level
/// drawn from a generator is exactly what a store whose index entries are
/// **derived rather than logged** cannot have: two replicas would build
/// different graphs from one log and disagree, silently, about which ten records
/// are nearest.
///
/// The byte is in the key anyway. Reserving room costs nothing today and cannot
/// be done retroactively — the same argument the key-kind table itself makes —
/// and it sorts before the record id so a future level's nodes group together.
///
/// # The vector is in the node
///
/// A walk visits many nodes and answers with few, so carrying the vector here
/// means the search touches index keys and decodes no records until the answer
/// is chosen. Measured on this store, decoding the records is about a seventh of
/// a linear nearest-neighbour read; here it is avoided as a side effect rather
/// than pursued as a feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorNodeKey {
    /// Which index this node belongs to.
    pub address: IndexAddress,
    /// Which layer. Always zero until the graph has more than one.
    pub level: u8,
    /// The record the node stands for.
    pub id: RecordId,
}

impl VectorNodeKey {
    /// Name one node.
    #[must_use]
    pub const fn new(address: IndexAddress, level: u8, id: RecordId) -> Self {
        Self { address, level, id }
    }

    /// The prefix every node of one level shares.
    #[must_use]
    pub fn level_prefix(address: &IndexAddress, level: u8) -> Vec<u8> {
        let mut bytes = address.prefix(KeyKind::VectorNode);
        bytes.push(level);
        bytes
    }
}

impl StoreKey for VectorNodeKey {
    type Value = VectorNode;

    const KIND: KeyKind = KeyKind::VectorNode;

    fn encode(&self) -> Key {
        let mut bytes = Self::level_prefix(&self.address, self.level);
        let mut writer = KeyWriter::new();
        record_id::put(&mut writer, &self.id);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let level = reader.take_u8()?;
        let id = record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self { address, level, id })
    }
}

/// What a node holds: the record's vector, and who it points at.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorNode {
    /// The record's vector, as the distance functions want it.
    pub vector: Vec<f64>,
    /// The records this node links to, in the order the graph chose.
    pub neighbours: Vec<RecordId>,
}

impl VectorNode {
    /// Build a node.
    #[must_use]
    pub const fn new(vector: Vec<f64>, neighbours: Vec<RecordId>) -> Self {
        Self { vector, neighbours }
    }
}

impl StoreValue for VectorNode {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u32(u32::try_from(self.vector.len()).unwrap_or(u32::MAX));
        for component in &self.vector {
            // The bit pattern, not a decimal projection: this is storage for
            // arithmetic rather than an index key, so nothing here has to sort.
            writer.put_u64(component.to_bits());
        }
        writer.put_u32(u32::try_from(self.neighbours.len()).unwrap_or(u32::MAX));
        for neighbour in &self.neighbours {
            record_id::put(&mut writer, neighbour);
        }
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::VectorNode, payload);
        let dimensions = reader.take_u32()?;
        let mut vector = Vec::with_capacity(usize::try_from(dimensions).unwrap_or(0));
        for _ in 0..dimensions {
            vector.push(f64::from_bits(reader.take_u64()?));
        }
        let count = reader.take_u32()?;
        let mut neighbours = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        for _ in 0..count {
            neighbours.push(record_id::take(&mut reader)?);
        }
        reader.finish()?;
        Ok(Self { vector, neighbours })
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
    type Value = Posting;

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
        reader.finish()?;
        Ok(Self::Counted { frequency, length })
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
        use super::{RecordId, VectorNode, VectorNodeKey};
        let node = VectorNode::new(
            vec![0.123, 0.999, 0.0, 1.0, 0.5],
            vec![RecordId::Int(1), RecordId::Int(2)],
        );
        let encoded = node.encode();
        let read = VectorNode::decode(encoded.as_slice()).expect("a node");
        assert_eq!(read, node);

        let key = VectorNodeKey::new(address(), 0, RecordId::Int(7));
        let bytes = key.encode();
        assert_eq!(VectorNodeKey::decode(bytes.as_slice()).expect("a key"), key);
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
}
