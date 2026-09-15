//! The value codec.
//!
//! Every stored value starts with a codec version. The first byte is format
//! metadata, not payload, and decode dispatches on it. Storing a bare serialized
//! struct instead and relying on the serializer's own tolerance works until the
//! day the format changes, at which point old bytes decode into a plausible
//! wrong value with no error.
//!
//! Layout, uniform across every value in the store:
//!
//! ```text
//! <codec-version:1> <flags:1> <payload…>
//! ```
//!
//! Flag bits that a value type does not define are reserved and must be zero. A
//! set reserved bit means the writer knew something this build does not, so it
//! is an error rather than something to ignore.

use tessari_kv::Value;
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, Sequence, TableId};

use crate::causal::CausalStamp;
use crate::error::{Error, Result};
use crate::node::NODE_ID_LEN;
use crate::order::{KeyReader, KeyWriter};

/// The codec version this build writes and reads.
pub const CODEC_VERSION: u8 = 1;

/// Bit 0 of the flags byte: the record was deleted at this version.
const FLAG_TOMBSTONE: u8 = 0b0000_0001;

/// Bit 1 of the flags byte: a log record names the leadership that wrote it.
///
/// A separate bit rather than bit 0 even though the flags byte is per value
/// type: a decode routed to the wrong type is a thing that happens, and two
/// meanings sharing one bit turn that mistake into a plausible wrong answer
/// instead of an error.
///
/// Clear means epoch zero, which is what every record written before there was
/// a cluster means, so nothing already on disk is rewritten. It is a flag and
/// not a codec-version bump because the version is shared by every value in the
/// store: bumping it to add a field to one of them would refuse all the others.
const FLAG_EPOCH: u8 = 0b0000_0010;

/// Bit 2 of the flags byte: this version carries the causal context its writer
/// had seen.
///
/// Its own bit for the reason bit 1 states: one bit per meaning is what keeps a
/// decode routed to the wrong value type an error rather than a plausible wrong
/// answer. Clear means an **empty** stamp, which is the honest reading of every
/// record written before there was a second master — nothing already on disk is
/// rewritten, exactly as when bit 1 arrived.
const FLAG_STAMP: u8 = 0b0000_0100;

/// Bytes of header that precede every payload.
const HEADER_LEN: usize = 2;

/// Bytes an epoch occupies when a log record carries one.
const EPOCH_LEN: usize = 8;

/// Bytes the entry count occupies in front of a stamp's entries.
const STAMP_COUNT_LEN: usize = 4;

/// Bytes one stamp entry occupies: the node, then its count.
const STAMP_ENTRY_LEN: usize = NODE_ID_LEN + 8;

/// A value that can be stored under a [`StoreKey`](crate::StoreKey).
pub trait StoreValue: Sized {
    /// Encode to the bytes written into the substrate.
    fn encode(&self) -> Value;

    /// Decode from bytes read out of the substrate.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are truncated, carry an unsupported
    /// codec version, or set a reserved flag bit.
    fn decode(bytes: &[u8]) -> Result<Self>;
}

/// Split the common header off a stored value.
pub(crate) fn split_header(bytes: &[u8], allowed_flags: u8) -> Result<(u8, &[u8])> {
    let header = bytes.get(..HEADER_LEN).ok_or(Error::ValueTruncated {
        len: bytes.len(),
        needed: HEADER_LEN,
    })?;
    let version = header[0];
    if version != CODEC_VERSION {
        return Err(Error::UnsupportedCodecVersion {
            found: version,
            supported: CODEC_VERSION,
        });
    }
    let flags = header[1];
    if flags & !allowed_flags != 0 {
        return Err(Error::ReservedFlags { flags });
    }
    let payload = bytes.get(HEADER_LEN..).unwrap_or_default();
    Ok((flags, payload))
}

/// Write the common header into a fresh buffer.
pub(crate) fn with_header(flags: u8, payload_len: usize) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(HEADER_LEN.saturating_add(payload_len));
    buffer.push(CODEC_VERSION);
    buffer.push(flags);
    buffer
}

/// One version of one record.
///
/// A deletion is a version, not an absence: under MVCC a reader at an older
/// sequence must still see the record, and a reader at a newer one must see that
/// it is gone. An absent key cannot express either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordValue {
    /// The record existed at this version, carrying this payload.
    ///
    /// The payload is opaque here — the document codec owns its interior.
    Present(Vec<u8>),
    /// The record was deleted at this version.
    Tombstone,
}

impl RecordValue {
    /// Whether this version marks a deletion.
    #[must_use]
    pub const fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone)
    }

    /// The payload, or an empty slice for a tombstone.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        match self {
            Self::Present(payload) => payload,
            Self::Tombstone => &[],
        }
    }
}

impl StoreValue for RecordValue {
    fn encode(&self) -> Value {
        match self {
            Self::Present(payload) => {
                let mut buffer = with_header(0, payload.len());
                buffer.extend_from_slice(payload);
                Value::from(buffer)
            }
            Self::Tombstone => Value::from(with_header(FLAG_TOMBSTONE, 0)),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (flags, payload) = split_header(bytes, FLAG_TOMBSTONE)?;
        if flags & FLAG_TOMBSTONE == 0 {
            return Ok(Self::Present(payload.to_vec()));
        }
        if payload.is_empty() {
            return Ok(Self::Tombstone);
        }
        Err(Error::TombstoneWithPayload { len: payload.len() })
    }
}

/// One version of one record as the store holds it: what the record became, and
/// the causal context its writer had **seen** when it wrote.
///
/// The stamp sits beside the version rather than inside it because a deletion is
/// as capable of being concurrent as a write is — a delete racing a write to one
/// record is one of the cases a multi-master range has to name, and an enum
/// variant could only carry the stamp *instead of* `Tombstone`, never alongside
/// it (Q-638).
///
/// It is also the only home the field needs. `Mutation` carries a version, and
/// the record store holds these same bytes under a [`RecordKey`], so one field
/// here reaches the log, the wire, a backup and the store at once. Putting it on
/// `Mutation` would have served the log and left the store's codec to change
/// again later, and ADR-0059's rule is that a format door closes once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StampedValue {
    value: RecordValue,
    stamp: CausalStamp,
}

impl StampedValue {
    /// A version written with no recorded causal context.
    ///
    /// The honest value for a store that has never had two masters, in the same
    /// way [`LogRecord::new`] is the honest value for one that has never elected
    /// anybody: an empty stamp leaves the flag bit clear, so nothing already
    /// written is rewritten and an older build's bytes decode unchanged.
    #[must_use]
    pub fn new(value: RecordValue) -> Self {
        Self {
            value,
            stamp: CausalStamp::new(),
        }
    }

    /// A version written by a node carrying what it had seen.
    #[must_use]
    pub const fn stamped(stamp: CausalStamp, value: RecordValue) -> Self {
        Self { value, stamp }
    }

    /// What the record became at this version.
    #[must_use]
    pub const fn value(&self) -> &RecordValue {
        &self.value
    }

    /// The causal context the writer had seen, empty when none was recorded.
    #[must_use]
    pub const fn stamp(&self) -> &CausalStamp {
        &self.stamp
    }

    /// Take the version, discarding the stamp.
    #[must_use]
    pub fn into_value(self) -> RecordValue {
        self.value
    }
}

/// Split the optional causal stamp off an encoded version.
///
/// One splitter for the same reason [`split_epoch`] gives: two readings of one
/// byte string is a thing that can come to disagree with itself, and here the
/// disagreement would be about whether two writes saw each other.
fn split_stamp(bytes: &[u8]) -> Result<(CausalStamp, bool, &[u8])> {
    let (flags, payload) = split_header(bytes, FLAG_TOMBSTONE | FLAG_STAMP)?;
    let tombstone = flags & FLAG_TOMBSTONE != 0;
    if flags & FLAG_STAMP == 0 {
        return Ok((CausalStamp::new(), tombstone, payload));
    }
    let raw: [u8; STAMP_COUNT_LEN] = payload
        .get(..STAMP_COUNT_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(STAMP_COUNT_LEN),
        })?;
    let count = u32::from_be_bytes(raw) as usize;
    let span = count.saturating_mul(STAMP_ENTRY_LEN);
    let body = payload
        .get(STAMP_COUNT_LEN..STAMP_COUNT_LEN.saturating_add(span))
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN
                .saturating_add(STAMP_COUNT_LEN)
                .saturating_add(span),
        })?;
    let mut entries = Vec::with_capacity(count);
    for entry in body.chunks_exact(STAMP_ENTRY_LEN) {
        let node: [u8; NODE_ID_LEN] = entry
            .get(..NODE_ID_LEN)
            .and_then(|head| head.try_into().ok())
            .ok_or(Error::ValueTruncated {
                len: bytes.len(),
                needed: STAMP_ENTRY_LEN,
            })?;
        let seen: [u8; 8] = entry
            .get(NODE_ID_LEN..)
            .and_then(|tail| tail.try_into().ok())
            .ok_or(Error::ValueTruncated {
                len: bytes.len(),
                needed: STAMP_ENTRY_LEN,
            })?;
        entries.push((node, u64::from_be_bytes(seen)));
    }
    Ok((
        CausalStamp::from_entries(entries)?,
        tombstone,
        payload
            .get(STAMP_COUNT_LEN.saturating_add(span)..)
            .unwrap_or_default(),
    ))
}

impl StoreValue for StampedValue {
    /// The stamp goes in front of the record's payload, not behind it.
    ///
    /// The same argument the epoch's position rests on: a reader that wants only
    /// the causal context reads a bounded prefix instead of walking a payload
    /// whose length it would otherwise have to learn from somewhere else.
    fn encode(&self) -> Value {
        let entries = self.stamp.entries();
        let mut flags = if self.value.is_tombstone() {
            FLAG_TOMBSTONE
        } else {
            0
        };
        if !entries.is_empty() {
            flags |= FLAG_STAMP;
        }
        let stamp_len = if entries.is_empty() {
            0
        } else {
            STAMP_COUNT_LEN.saturating_add(entries.len().saturating_mul(STAMP_ENTRY_LEN))
        };
        let payload = self.value.payload();
        let mut buffer = with_header(flags, stamp_len.saturating_add(payload.len()));
        if !entries.is_empty() {
            buffer.extend_from_slice(
                &u32::try_from(entries.len())
                    .unwrap_or(u32::MAX)
                    .to_be_bytes(),
            );
            for (node, seen) in entries {
                buffer.extend_from_slice(node);
                buffer.extend_from_slice(&seen.to_be_bytes());
            }
        }
        buffer.extend_from_slice(payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (stamp, tombstone, payload) = split_stamp(bytes)?;
        if !tombstone {
            return Ok(Self {
                value: RecordValue::Present(payload.to_vec()),
                stamp,
            });
        }
        if payload.is_empty() {
            return Ok(Self {
                value: RecordValue::Tombstone,
                stamp,
            });
        }
        Err(Error::TombstoneWithPayload { len: payload.len() })
    }
}

/// One mutation inside a log record: which record, and what it becomes.
///
/// It carries the record's **address**, not its encoded key. An encoded key has
/// the version baked into it, so a record whose embedded version disagreed with
/// its own log sequence would create a second ordering authority — the thing
/// ADR-0001 exists to prevent. Carrying the address and deriving the version
/// from the log entry's own sequence makes that disagreement unrepresentable
/// rather than merely forbidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
    /// What the record becomes at this sequence, and what its writer had seen.
    pub value: StampedValue,
}

/// Everything one commit changed, as the log carries it.
///
/// Mutations are stored in address order. That is not tidiness: a deterministic
/// apply has to be fed a deterministic record, so the *encoder* is bound by the
/// same no-unordered-iteration rule the apply path is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogRecord {
    epoch: Epoch,
    mutations: Vec<Mutation>,
}

impl LogRecord {
    /// Build a record from mutations that are already in address order.
    ///
    /// Sorting happens here rather than being assumed, because a caller that
    /// hands over an unordered collection would produce a record that replays to
    /// the same state but not to the same bytes — and byte-identical replay is
    /// the property being protected.
    #[must_use]
    pub fn new(mutations: Vec<Mutation>) -> Self {
        Self::at(Epoch::ZERO, mutations)
    }

    /// Build a record written under a named leadership.
    ///
    /// `new` is the same call at [`Epoch::ZERO`], which is the honest value for
    /// a store that has never elected anybody: this build allocates no epochs,
    /// so every record it writes belongs to the first and only leadership.
    #[must_use]
    pub fn at(epoch: Epoch, mut mutations: Vec<Mutation>) -> Self {
        mutations.sort_by(|left, right| {
            (left.namespace, left.database, left.table, &left.id).cmp(&(
                right.namespace,
                right.database,
                right.table,
                &right.id,
            ))
        });
        Self { epoch, mutations }
    }

    /// The leadership that wrote this record.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The mutations this record applies, in address order.
    #[must_use]
    pub fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }

    /// Read the leadership out of encoded bytes without decoding the record.
    ///
    /// This is what the fixed offset was bought for. A store comparing the
    /// record it holds at a position against the one it is offered needs one
    /// number, and decoding the whole record to get it would put the cost of
    /// every mutation on a path that exists to be cheap.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are truncated, carry an unsupported
    /// codec version, or set a reserved flag bit.
    pub fn epoch_in(bytes: &[u8]) -> Result<Epoch> {
        Ok(split_epoch(bytes)?.0)
    }
}

/// Split the optional epoch off an encoded log record.
///
/// One splitter rather than one in `decode` and another in `epoch_in`: two
/// readings of the same bytes is a thing that can disagree with itself, which is
/// the reason this record carries no mutation count either.
fn split_epoch(bytes: &[u8]) -> Result<(Epoch, &[u8])> {
    let (flags, payload) = split_header(bytes, FLAG_EPOCH)?;
    if flags & FLAG_EPOCH == 0 {
        return Ok((Epoch::ZERO, payload));
    }
    let raw: [u8; EPOCH_LEN] = payload
        .get(..EPOCH_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(EPOCH_LEN),
        })?;
    Ok((
        Epoch::new(u64::from_be_bytes(raw)),
        payload.get(EPOCH_LEN..).unwrap_or_default(),
    ))
}

impl StoreValue for LogRecord {
    /// There is deliberately no mutation count in front of the mutations.
    ///
    /// Every mutation is self-delimiting — the record id is terminated and the
    /// value is length-prefixed — so a count would be a second statement of the
    /// same fact, which is a thing that can disagree with itself. It would also
    /// need a width, and a width needs a policy for what happens when a commit
    /// exceeds it. Neither question has to be answered if the field does not
    /// exist.
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::with_capacity(self.mutations.len().saturating_mul(32));
        for mutation in &self.mutations {
            writer
                .put_u32(mutation.namespace.get())
                .put_u32(mutation.database.get())
                .put_u32(mutation.table.get());
            crate::record_id::put(&mut writer, &mutation.id);
            let encoded = mutation.value.encode();
            writer
                .put_u32(length_of(encoded.as_slice()))
                .put_fixed(encoded.as_slice());
        }
        let payload = writer.finish();
        // The epoch goes in front of the mutations rather than into them, so a
        // reader that only wants to know which leadership wrote this record
        // reads a fixed offset instead of walking every mutation in it.
        let (flags, epoch) = if self.epoch == Epoch::ZERO {
            (0, None)
        } else {
            (FLAG_EPOCH, Some(self.epoch.get().to_be_bytes()))
        };
        let mut buffer = with_header(flags, payload.len().saturating_add(EPOCH_LEN));
        if let Some(epoch) = epoch {
            buffer.extend_from_slice(&epoch);
        }
        buffer.extend_from_slice(&payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (epoch, payload) = split_epoch(bytes)?;
        // The reader is the crate's bounds-checked byte cursor. The kind it is
        // built with only names the entity in a truncation error, and no kind
        // tag is consumed here — this is a value payload, not a key.
        let mut reader = KeyReader::new(crate::kind::KeyKind::LogEntry, payload);
        let mut mutations = Vec::new();
        while reader.remaining() > 0 {
            let namespace = NamespaceId::new(reader.take_u32()?);
            let database = DatabaseId::new(reader.take_u32()?);
            let table = TableId::new(reader.take_u32()?);
            let id = crate::record_id::take(&mut reader)?;
            let len = reader.take_u32()?;
            let encoded = reader.take_exact(usize::try_from(len).unwrap_or(usize::MAX))?;
            mutations.push(Mutation {
                namespace,
                database,
                table,
                id,
                value: StampedValue::decode(&encoded)?,
            });
        }
        Ok(Self { epoch, mutations })
    }
}

/// A byte length as it is written into a log record.
///
/// A value longer than a `u32` can express is not something this store can
/// produce — a single record that large would have failed on memory long before
/// reaching the codec — and saturating here keeps the encoder total rather than
/// making every caller of `encode` handle a case that cannot arise. The decoder
/// would reject the result as truncated, so the failure is loud either way.
fn length_of(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap_or(u32::MAX)
}

/// The store's own on-disk format version.
///
/// Independent of any storage engine's internal versioning: this one describes
/// the key grammar and the value codec, which are ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FormatVersion(u32);

impl FormatVersion {
    /// The format this build writes.
    ///
    /// Moved to 2 when the log record gained its epoch (ADR-0059). A store
    /// already on version 1 is still opened and is not rewritten — its records
    /// read as the first leadership, which is what they are — so the bump buys
    /// one thing: a store *created* by this build is refused by an older one at
    /// `open`, rather than at whichever read first meets a flag bit it does not
    /// know.
    ///
    /// Moved to 3 when the log became per-range and its keys gained a home. A
    /// store below 3 **is** rewritten at open, which is the difference from the
    /// bump before it: version 2 changed what a value means, and an old value
    /// still decoded; version 3 changed the shape of a key, and an old key does
    /// not decode at all. Keeping two key decoders live would put the choice
    /// between them on the replication read path forever, for a format nothing
    /// has released.
    ///
    /// Moved to 4 when a stored version gained its causal stamp. This is the
    /// version-2 shape and not the version-3 one: the bump changes what a value
    /// *means* while every value already written still decodes, because a clear
    /// stamp bit reads as an empty stamp. So a store below 4 is opened and is
    /// **not** rewritten, and the bump buys the same single thing the first one
    /// did — a store created by this build is refused by an older one at `open`
    /// rather than at whichever read first meets a flag bit it does not know.
    ///
    /// Moved to 5 when a log key gained its writer (G027 S2.2). Unlike the three
    /// bumps before it, a store below 5 **is** rewritten at open: the writer is
    /// a fixed-width field in the key rather than a flag bit in a value, so an
    /// old key and a new one differ in length, and telling two key shapes apart
    /// by their length on the replication read path is a choice made on every
    /// record forever rather than once. `give_an_older_log_its_home` already
    /// refused that trade for the home; this is the same refusal for the writer.
    pub const CURRENT: Self = Self(5);

    /// The first format whose log keys carry a home.
    ///
    /// Named rather than written as a literal at the one place that compares
    /// against it: a later bump moves `CURRENT` and must leave this where it is,
    /// and a literal `3` sitting in the storage layer would move with whichever
    /// of the two the next author happened to be reading.
    pub const HOMED_LOG: Self = Self(3);

    /// The first format whose log keys carry the writer that allocated them.
    ///
    /// Named for the reason [`Self::HOMED_LOG`] is named.
    pub const WRITER_QUALIFIED_LOG: Self = Self(5);

    /// Wrap a raw format version.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw format version.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Refuse a store written by a newer build.
    ///
    /// Opening forward-compatibly would write this build's format into a store
    /// that already holds a newer one, which is not recoverable afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedFormatVersion`] when the store is newer than
    /// this build.
    pub fn check_supported(self) -> Result<()> {
        if self.0 > Self::CURRENT.0 {
            return Err(Error::UnsupportedFormatVersion {
                found: self.0,
                supported: Self::CURRENT.0,
            });
        }
        Ok(())
    }
}

impl StoreValue for FormatVersion {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, 4);
        buffer.extend_from_slice(&self.0.to_be_bytes());
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let raw: [u8; 4] = payload.try_into().map_err(|_| Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(4),
        })?;
        Ok(Self(u32::from_be_bytes(raw)))
    }
}

impl StoreValue for Sequence {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, 8);
        buffer.extend_from_slice(&self.get().to_be_bytes());
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let raw: [u8; 8] = payload.try_into().map_err(|_| Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(8),
        })?;
        Ok(Self::new(u64::from_be_bytes(raw)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    const ONE_NODE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
    const ANOTHER_NODE: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

    fn a_stamp(nodes: &[([u8; NODE_ID_LEN], u64)]) -> CausalStamp {
        let mut stamp = CausalStamp::new();
        for (node, times) in nodes {
            for _ in 0..*times {
                stamp.advance(*node);
            }
        }
        stamp
    }

    #[test]
    fn a_stamped_version_round_trips_both_halves() {
        let stamp = a_stamp(&[(ONE_NODE, 3), (ANOTHER_NODE, 1)]);
        let value = StampedValue::stamped(stamp.clone(), RecordValue::Present(b"payload".to_vec()));
        let decoded = StampedValue::decode(value.encode().as_slice()).unwrap();
        assert_eq!(&decoded, &value);
        assert_eq!(decoded.stamp(), &stamp);
        assert_eq!(decoded.value().payload(), b"payload");
    }

    /// A delete racing a write is one of the cases a multi-master range has to
    /// name, so the stamp has to survive on a version that carries no payload.
    /// This is the case a third enum variant could not have expressed (Q-638).
    #[test]
    fn a_tombstone_carries_a_stamp_too() {
        let stamp = a_stamp(&[(ANOTHER_NODE, 2)]);
        let value = StampedValue::stamped(stamp.clone(), RecordValue::Tombstone);
        let decoded = StampedValue::decode(value.encode().as_slice()).unwrap();
        assert!(decoded.value().is_tombstone());
        assert_eq!(decoded.stamp(), &stamp);
    }

    /// The bump is a promise that nothing already written is rewritten, and this
    /// is what makes the promise checkable rather than asserted.
    #[test]
    fn an_unstamped_version_encodes_exactly_as_it_did_before_the_stamp() {
        for value in [
            RecordValue::Present(b"payload".to_vec()),
            RecordValue::Present(Vec::new()),
            RecordValue::Tombstone,
        ] {
            let before = value.encode();
            let after = StampedValue::new(value.clone()).encode();
            assert_eq!(after.as_slice(), before.as_slice());
            let decoded = StampedValue::decode(before.as_slice()).unwrap();
            assert_eq!(decoded.value(), &value);
            assert!(decoded.stamp().is_empty());
        }
    }

    /// Both readers of a stamped value have to agree, and the older one cannot
    /// agree by guessing: it has never seen bit 2, so it refuses loudly instead
    /// of returning a version stripped of the context that says who saw what.
    #[test]
    fn the_unstamped_reader_refuses_a_stamped_value_rather_than_dropping_it() {
        let encoded = StampedValue::stamped(
            a_stamp(&[(ONE_NODE, 1)]),
            RecordValue::Present(b"payload".to_vec()),
        )
        .encode();
        let error = RecordValue::decode(encoded.as_slice()).unwrap_err();
        assert!(matches!(error, Error::ReservedFlags { flags } if flags & FLAG_STAMP != 0));
    }

    /// Every read of a stamp binary-searches its entries, so an unordered list
    /// answers the wrong node's count instead of failing. The decoder is the
    /// only place the invariant can break, so it is the only place that checks.
    #[test]
    fn entries_out_of_node_order_are_refused_rather_than_sorted() {
        let mut bytes = vec![CODEC_VERSION, FLAG_STAMP];
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        for node in [ANOTHER_NODE, ONE_NODE] {
            bytes.extend_from_slice(&node);
            bytes.extend_from_slice(&1_u64.to_be_bytes());
        }
        assert!(matches!(
            StampedValue::decode(&bytes).unwrap_err(),
            Error::StampOutOfOrder { at: 1 }
        ));
    }

    #[test]
    fn a_stamp_cut_short_of_its_own_entry_count_is_truncated_not_short() {
        let mut bytes = vec![CODEC_VERSION, FLAG_STAMP];
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&ONE_NODE);
        bytes.extend_from_slice(&1_u64.to_be_bytes());
        assert!(matches!(
            StampedValue::decode(&bytes).unwrap_err(),
            Error::ValueTruncated { .. }
        ));
    }

    /// The log is the first of S1.3's three carriers, and it carries the stamp
    /// by carrying the version — there is no second field to keep in step.
    #[test]
    fn a_stamp_survives_the_log_record_that_carries_the_version() {
        let stamp = a_stamp(&[(ONE_NODE, 7), (ANOTHER_NODE, 2)]);
        let record = LogRecord::new(vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("a"),
            value: StampedValue::stamped(stamp.clone(), RecordValue::Present(b"v".to_vec())),
        }]);
        let encoded = record.encode();
        let decoded = LogRecord::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(decoded.mutations()[0].value.stamp(), &stamp);
        assert_eq!(
            LogRecord::decode(encoded.as_slice())
                .unwrap()
                .encode()
                .as_slice(),
            encoded.as_slice()
        );
    }

    #[test]
    fn a_present_record_round_trips_its_payload() {
        let value = RecordValue::Present(b"payload".to_vec());
        let encoded = value.encode();
        assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
        assert_eq!(RecordValue::decode(encoded.as_slice()).unwrap(), value);
    }

    #[test]
    fn a_tombstone_is_a_version_with_no_payload() {
        let encoded = RecordValue::Tombstone.encode();
        assert_eq!(encoded.as_slice(), &[CODEC_VERSION, FLAG_TOMBSTONE]);
        let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
        assert!(decoded.is_tombstone());
        assert!(decoded.payload().is_empty());
    }

    #[test]
    fn an_empty_present_record_is_not_a_tombstone() {
        let encoded = RecordValue::Present(Vec::new()).encode();
        let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
        assert!(!decoded.is_tombstone());
    }

    #[test]
    fn an_unknown_codec_version_is_refused_rather_than_guessed() {
        let error = RecordValue::decode(&[9, 0]).unwrap_err();
        assert!(matches!(
            error,
            Error::UnsupportedCodecVersion {
                found: 9,
                supported: CODEC_VERSION
            }
        ));
        assert_eq!(error.code(), "incompatible");
    }

    #[test]
    fn a_reserved_flag_bit_is_refused() {
        let error = RecordValue::decode(&[CODEC_VERSION, 0b0000_0010]).unwrap_err();
        assert!(matches!(error, Error::ReservedFlags { flags: 0b10 }));
    }

    #[test]
    fn a_tombstone_bit_on_a_meta_value_is_reserved_there() {
        let error =
            FormatVersion::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0, 0, 0, 1]).unwrap_err();
        assert!(matches!(error, Error::ReservedFlags { .. }));
    }

    #[test]
    fn a_tombstone_carrying_payload_contradicts_itself() {
        let error = RecordValue::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0xAA]).unwrap_err();
        assert!(matches!(error, Error::TombstoneWithPayload { len: 1 }));
    }

    #[test]
    fn a_value_shorter_than_its_header_is_truncated() {
        assert!(matches!(
            RecordValue::decode(&[CODEC_VERSION]).unwrap_err(),
            Error::ValueTruncated { len: 1, needed: 2 }
        ));
    }

    #[test]
    fn the_format_version_round_trips_and_refuses_newer_stores() {
        let encoded = FormatVersion::CURRENT.encode();
        assert_eq!(
            FormatVersion::decode(encoded.as_slice()).unwrap(),
            FormatVersion::CURRENT
        );
        assert!(FormatVersion::CURRENT.check_supported().is_ok());
        // Derived from CURRENT rather than written as a literal: a version this
        // test restates is a version this test stops checking the moment the
        // format moves.
        match FormatVersion::new(99).check_supported().unwrap_err() {
            Error::UnsupportedFormatVersion { found, supported } => {
                assert_eq!(found, 99);
                assert_eq!(supported, FormatVersion::CURRENT.get());
            }
            other => panic!("expected an unsupported-format error, got {other:?}"),
        }
    }

    #[test]
    fn a_sequence_round_trips_as_a_stored_value() {
        let sequence = Sequence::new(1_234_567);
        let encoded = sequence.encode();
        assert_eq!(Sequence::decode(encoded.as_slice()).unwrap(), sequence);
    }

    fn mutation(id: RecordId, value: RecordValue) -> Mutation {
        Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            table: TableId::new(3),
            id,
            value: StampedValue::new(value),
        }
    }

    #[test]
    fn a_log_record_round_trips_every_mutation_shape() {
        let record = LogRecord::new(vec![
            mutation(RecordId::Int(-42), RecordValue::Present(b"a".to_vec())),
            mutation(RecordId::from("text"), RecordValue::Tombstone),
            mutation(RecordId::Uuid([0x5a; 16]), RecordValue::Present(Vec::new())),
            mutation(
                RecordId::Bytes(vec![0x00, 0xff, 0x00]),
                RecordValue::Present(vec![0x00; 300]),
            ),
        ]);
        let encoded = record.encode();
        assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
        assert_eq!(LogRecord::decode(encoded.as_slice()).unwrap(), record);
    }

    #[test]
    fn an_empty_log_record_round_trips_as_the_header_alone() {
        let record = LogRecord::new(Vec::new());
        assert!(record.mutations().is_empty());
        let encoded = record.encode();
        assert_eq!(encoded.as_slice(), &[CODEC_VERSION, 0]);
        assert_eq!(LogRecord::decode(encoded.as_slice()).unwrap(), record);
    }

    #[test]
    fn mutations_are_stored_in_address_order_whatever_order_they_arrive_in() {
        // Byte-identical replay depends on the encoder being deterministic, not
        // only on the apply path being deterministic. A caller that hands over
        // an unordered collection must not be able to change the bytes.
        let ordered = LogRecord::new(vec![
            mutation(RecordId::from("a"), RecordValue::Tombstone),
            mutation(RecordId::from("b"), RecordValue::Tombstone),
            mutation(RecordId::from("c"), RecordValue::Tombstone),
        ]);
        let shuffled = LogRecord::new(vec![
            mutation(RecordId::from("c"), RecordValue::Tombstone),
            mutation(RecordId::from("a"), RecordValue::Tombstone),
            mutation(RecordId::from("b"), RecordValue::Tombstone),
        ]);
        assert_eq!(ordered, shuffled);
        assert_eq!(
            ordered.encode().as_slice(),
            shuffled.encode().as_slice(),
            "the same mutation set must encode to the same bytes"
        );
    }

    #[test]
    fn mutations_sort_by_the_whole_address_and_not_only_by_record_id() {
        let record = LogRecord::new(vec![
            Mutation {
                namespace: NamespaceId::new(2),
                database: DatabaseId::new(1),
                table: TableId::new(1),
                id: RecordId::from("a"),
                value: StampedValue::new(RecordValue::Tombstone),
            },
            Mutation {
                namespace: NamespaceId::new(1),
                database: DatabaseId::new(1),
                table: TableId::new(1),
                id: RecordId::from("z"),
                value: StampedValue::new(RecordValue::Tombstone),
            },
        ]);
        assert_eq!(record.mutations()[0].namespace, NamespaceId::new(1));
    }

    #[test]
    fn a_log_record_truncated_mid_mutation_is_refused_rather_than_half_decoded() {
        let record = LogRecord::new(vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"payload".to_vec()),
        )]);
        let full = record.encode();
        let bytes = full.as_slice();
        // From one byte past the header: a cut exactly at the header is not a
        // truncated record, it is an empty one, and that is legitimate.
        for cut in HEADER_LEN.saturating_add(1)..bytes.len() {
            assert!(
                LogRecord::decode(&bytes[..cut]).is_err(),
                "a record cut at {cut} decoded anyway"
            );
        }
    }

    #[test]
    fn a_value_length_naming_more_bytes_than_exist_is_refused() {
        // The length comes out of the payload, so it is not trusted.
        let mut bytes = LogRecord::new(vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"x".to_vec()),
        )])
        .encode()
        .into_bytes();
        let last = bytes.len().saturating_sub(3);
        bytes[last] = 0xff;
        assert!(LogRecord::decode(&bytes).is_err());
    }

    #[test]
    fn a_record_at_the_first_leadership_is_byte_identical_to_one_written_before_epochs_existed() {
        // The whole point of spending a flag bit rather than widening every
        // record: a store that has never elected anybody keeps the bytes it
        // already has, so no existing log entry is rewritten and byte-identical
        // replay across builds survives the format change.
        let record = LogRecord::new(vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"v".to_vec()),
        )]);
        assert_eq!(record.epoch(), Epoch::ZERO);
        let bytes = record.encode().into_bytes();
        assert_eq!(bytes[1], 0, "no flag bit is set at the first leadership");
        assert_eq!(
            LogRecord::decode(&bytes).unwrap(),
            record,
            "and it decodes back to itself"
        );
    }

    #[test]
    fn a_record_carries_its_epoch_through_a_round_trip() {
        let record = LogRecord::at(
            Epoch::new(0x0102_0304_0506_0708),
            vec![mutation(
                RecordId::from("r"),
                RecordValue::Present(b"v".to_vec()),
            )],
        );
        let decoded = LogRecord::decode(record.encode().as_slice()).unwrap();
        assert_eq!(decoded.epoch(), Epoch::new(0x0102_0304_0506_0708));
        assert_eq!(decoded.mutations(), record.mutations());
        assert_eq!(decoded, record);
    }

    #[test]
    fn the_epoch_sits_in_front_of_the_mutations_so_a_reader_need_not_scan() {
        let bytes = LogRecord::at(
            Epoch::new(0x0102_0304_0506_0708),
            vec![mutation(
                RecordId::from("r"),
                RecordValue::Present(b"v".to_vec()),
            )],
        )
        .encode()
        .into_bytes();
        assert_eq!(bytes[1], FLAG_EPOCH);
        assert_eq!(
            &bytes[HEADER_LEN..HEADER_LEN + 8],
            &0x0102_0304_0506_0708_u64.to_be_bytes(),
            "fixed width, big-endian, immediately after the header"
        );
    }

    #[test]
    fn a_record_written_before_epochs_existed_decodes_as_the_first_leadership() {
        // Not a round trip: these are bytes as an older build wrote them, with
        // the flags byte clear and no epoch field at all.
        // A literal, not a round trip: these are the bytes an older build
        // wrote — flags clear, the mutations starting immediately after the
        // header — and a round trip against today's encoder could not tell the
        // difference if the decoder silently required an epoch.
        let legacy = [
            CODEC_VERSION,
            0, // flags: no epoch field follows
            0,
            0,
            0,
            1, // namespace
            0,
            0,
            0,
            2, // database
            0,
            0,
            0,
            3, // table
            0x02,
            b'r',
            0x00,
            0x01, // a string record id, terminated
            0,
            0,
            0,
            3, // the value's length
            CODEC_VERSION,
            0,
            b'v', // the value
        ];
        let decoded = LogRecord::decode(&legacy).unwrap();
        assert_eq!(decoded.epoch(), Epoch::ZERO);
        assert_eq!(decoded.mutations().len(), 1);
        assert_eq!(decoded.mutations()[0].id, RecordId::from("r"));
    }

    #[test]
    fn a_record_claiming_an_epoch_it_did_not_write_is_truncated_not_guessed() {
        let mut bytes = LogRecord::new(Vec::new()).encode().into_bytes();
        bytes[1] = FLAG_EPOCH;
        assert!(
            LogRecord::decode(&bytes).is_err(),
            "the flag promises eight bytes that are not there"
        );
    }
}
