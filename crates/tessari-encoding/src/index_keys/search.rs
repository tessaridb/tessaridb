//! Keys for full-text postings, terms and the statistics a score is measured against.

use super::{IndexAddress, IndexValues, Posting, SecondaryIndexKey, record_id, take_values};
use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::value::{StoreValue, split_header, with_header};
use tessari_kv::{Key, Value};
use tessari_types::RecordId;

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

/// One distinct term of one search index — the dictionary entry.
///
/// # Why a keyspace for something the postings already imply
///
/// The set of terms an index holds is derivable: walk every posting and take the
/// distinct prefixes. That is precisely the problem. Enumerating the terms under
/// `vect` today means walking every posting of `vector`, `vectors`, `vectorised`
/// and everything else that starts that way — work proportional to how many
/// *records* hold those words, to answer a question about *words*.
///
/// One entry per term makes three things bounded that are not bounded without
/// it, which is what earns a keyspace rather than a derived read:
///
/// - a **prefix** is a range read over distinct terms;
/// - a **fuzzy** match is an automaton intersected with an ordered walk of them;
/// - a term's **document frequency** is a point read, where it is currently a
///   scan of the term's whole posting range counted entry by entry — already the
///   dominant cost of a ranked read, once per query term per query.
///
/// The key is `address | term` with no record suffix, so a term has exactly one
/// entry and finding it is a point read. It sorts beside its own postings only
/// by accident of the address prefix; the kinds are separate bytes, so the
/// dictionary can be scanned without touching a single posting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTermKey {
    /// Which index this term belongs to.
    pub address: IndexAddress,
    /// The term, order-encoded exactly as the posting encodes it.
    ///
    /// The same encoding deliberately: a dictionary entry that spelled its term
    /// differently from the postings beside it would be a second vocabulary, and
    /// a prefix walk over one would not reach the other.
    pub term: IndexValues,
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
pub(crate) fn approximate(count: u64) -> f64 {
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

/// What one search index knows about one term.
///
/// # One number, and where the second one goes
///
/// `documents` is the term's document frequency — how many records hold it. It
/// is maintained rather than counted for the reason the collection statistics
/// are: counting it means walking the term's whole posting range, and a ranked
/// read does that once per query term, per query.
///
/// The other two numbers are the **postings' own extremes**, and they are the
/// upper bound safe top-k pruning prunes against.
///
/// # Why extremes rather than the impact the format specification named
///
/// The specification placed a single `max_impact` here: the largest contribution
/// any one posting can make to a score. A contribution is
/// `idf × saturation(occurrences, length, average_length)`, and
/// `average_length` belongs to the **collection**, which moves on every write.
/// So a stored impact is a number about a collection that no longer exists — and
/// the direction is the fatal part. When the average grows, the same posting
/// scores *higher*, so a stored impact becomes an **under**estimate. An
/// underestimated upper bound is not loose, it is unsound: the term is pruned and
/// the records it would have won are silently missing from the answer.
///
/// These two are properties of the postings alone and are sound at every
/// collection state, because saturation is increasing in occurrences and
/// decreasing in length. So for any posting `p` of this term and any average,
/// `saturation(f_p, dl_p) ≤ saturation(max_frequency, min_length)`. The pairing
/// takes the frequency from one record and the length from another, which is why
/// the bound is looser than a true maximum — and loose in the safe direction.
/// **ADR-0050.**
///
/// # The maintenance rule
///
/// `max_frequency` never falls and `min_length` never rises, for the reason the
/// specification already gave: an extreme cannot move inward without knowing the
/// second one, so neither is relaxed when a posting leaves. Both drift loose over
/// time, which costs pruning efficiency and never correctness, and a rebuild
/// recomputes them from the postings it writes. Both are integers, so they are
/// exact, they accumulate no float error across a release, and they compare with
/// `>` rather than against a tolerance.
///
/// # Zero means no bound, which means do not prune
///
/// A real posting has at least one occurrence and its record at least one token,
/// so neither number is ever legitimately zero. An entry written before these
/// existed decodes as `0, 0`, and that pair means **this term has no usable
/// bound** — a reader must decline to prune it rather than treat the bound as
/// zero, which would prune everything. [`Self::bound`] is the only way to ask,
/// and it answers `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermStatistics {
    /// How many records hold this term.
    pub documents: u64,
    /// The most occurrences any one posting of this term records.
    ///
    /// `0` when no bound has been recorded — see the type documentation.
    pub max_frequency: u32,
    /// The fewest tokens held by any record posting this term.
    ///
    /// `0` when no bound has been recorded — see the type documentation.
    pub min_length: u32,
}

impl TermStatistics {
    /// State the frequency, with no bound recorded.
    #[must_use]
    pub const fn new(documents: u64) -> Self {
        Self {
            documents,
            max_frequency: 0,
            min_length: 0,
        }
    }

    /// State the frequency and the postings' extremes.
    #[must_use]
    pub const fn bounded(documents: u64, max_frequency: u32, min_length: u32) -> Self {
        Self {
            documents,
            max_frequency,
            min_length,
        }
    }

    /// The extremes to score an upper bound from, when there are any.
    ///
    /// `None` is not "no records" — it is **this build cannot bound this term**,
    /// which obliges a caller to score the term's postings rather than prune
    /// them. Returning a zero pair instead would be an upper bound of zero, and
    /// a term that can contribute nothing is precisely a term to prune away.
    /// Every entry written before the bound existed is in this state, so the
    /// distinction is what lets an older index be read at all.
    #[must_use]
    pub const fn bound(&self) -> Option<(u32, u32)> {
        if self.max_frequency == 0 || self.min_length == 0 {
            return None;
        }
        Some((self.max_frequency, self.min_length))
    }
}

impl StoreValue for TermStatistics {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u64(self.documents);
        // Written unconditionally, including as the zero pair. Omitting them
        // when unset would make the payload's length mean two things — an entry
        // from an older build, and a current one with nothing to bound — and a
        // reader cannot tell those apart from the bytes. They are the same
        // *answer* (do not prune), but making one shape carry both meanings is
        // how the next field to arrive here becomes ambiguous.
        writer.put_u32(self.max_frequency);
        writer.put_u32(self.min_length);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SearchTerm, payload);
        let documents = reader.take_u64()?;
        // The payload dispatches on its own length, which is what the earlier
        // comment here anticipated: an entry written before the bound existed
        // ends after the count, and reads back as the zero pair — no bound, so
        // do not prune. Trailing bytes beyond the pair are still refused, so the
        // NEXT field to arrive is a payload this build declines rather than one
        // it misreads as this shape.
        let (max_frequency, min_length) = if reader.remaining() > 0 {
            (reader.take_u32()?, reader.take_u32()?)
        } else {
            (0, 0)
        };
        reader.finish()?;
        Ok(Self {
            documents,
            max_frequency,
            min_length,
        })
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

impl SearchTermKey {
    /// Name one term of one index.
    #[must_use]
    pub const fn new(address: IndexAddress, term: IndexValues) -> Self {
        Self { address, term }
    }

    /// The prefix every term of one index shares.
    ///
    /// Bounding a scan with this walks the whole dictionary and nothing else —
    /// no postings, no statistics, no entries of another index.
    #[must_use]
    pub fn dictionary_prefix(address: &IndexAddress) -> Vec<u8> {
        address.prefix(KeyKind::SearchTerm)
    }

    /// The prefix shared by every term beginning with these bytes.
    ///
    /// Exact rather than approximate, for the reason
    /// `IndexValues::string_prefix` gives: a string encodes as its tag, its
    /// escaped bytes and a terminator, and the escape is byte-local — so the
    /// entries under this prefix are exactly the terms beginning with it, and
    /// nothing has to be filtered out afterwards.
    #[must_use]
    pub fn term_prefix(address: &IndexAddress, prefix: &str) -> Vec<u8> {
        let mut bytes = Self::dictionary_prefix(address);
        bytes.extend_from_slice(&IndexValues::string_prefix(prefix));
        bytes
    }
}

impl StoreKey for SearchTermKey {
    type Value = TermStatistics;

    const KIND: KeyKind = KeyKind::SearchTerm;

    fn encode(&self) -> Key {
        let mut bytes = Self::dictionary_prefix(&self.address);
        bytes.extend_from_slice(self.term.as_slice());
        Key::from(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let term = take_values(&mut reader)?;
        reader.finish()?;
        Ok(Self { address, term })
    }
}
