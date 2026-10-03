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

use crate::causal::CausalStamp;
use crate::error::{Error, Result};
use crate::node::NODE_ID_LEN;
use crate::order::{KeyReader, KeyWriter};
pub use across::{
    Across, Decision, Part, Participant, Provenance, TRANSACTION_ID_LEN, TransactionId,
    TransactionRecord,
};
pub use format::FormatVersion;
pub use log_record::{LogRecord, Mutation};

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

/// Bit 3 of the flags byte, on a log record only: every mutation carries the
/// shard of its table it falls in (G031, ADR-0080).
///
/// The writer decides the shard at commit, from the catalog it committed
/// against, and writes the answer here so that everything reading the record
/// later — a follower applying it, a stream filter, `home_of` — asks the bytes
/// rather than a catalog that has moved on since. Set only when some mutation
/// is in a split table, so a record touching none keeps the bytes it always had;
/// a build that predates the bit refuses such a record as reserved rather than
/// reading its shard field as the next mutation's namespace.
const FLAG_SHARDS: u8 = 0b0000_1000;

/// Bit 4 of the flags byte, on a log record only: the record carries its
/// writer's commit order (G034, ADR-0084).
///
/// One writer files its commits in the log of each commit's home, so its one
/// order is spread over several logs and no position in one says anything about
/// a position in another. The writer is the only node that knows the order, so
/// it writes it here — its store version at commit, which already orders every
/// commit it makes — and a follower applies one writer's records across logs in
/// it. Without it a later commit filed in a coarser log was applied before an
/// earlier one filed in a finer log, and the follower ended at the older value
/// (Q-796). A record without the bit keeps the bytes it always had.
const FLAG_ORDER: u8 = 0b0001_0000;

/// Bit 5 of the flags byte, on a record version only: the version stops being
/// answered at a stated instant (G035).
///
/// Eight bytes of milliseconds since the Unix epoch follow the causal stamp and
/// precede the payload. The instant is data rather than a rule, so every node
/// and every reader computes the same visibility from the same bytes against
/// its own clock; see [`expiry`] for why a reader compares rather than a sweep
/// deciding. Clear means the version never expires, which is every version
/// written before this bit existed, so nothing on disk is rewritten.
const FLAG_EXPIRES: u8 = 0b0010_0000;

/// Bit 6 of the flags byte: the value belongs to a transaction across leaders
/// (ADR-0112) — on a log record, the section saying which record of it this is;
/// on a record version, the transaction it was resolved from.
///
/// One meaning on both types, written the same way first (the transaction id),
/// so a decode routed to the wrong type still reads a transaction rather than
/// a plausible something else. Clear means no such transaction, which is every
/// value written before this bit existed, so nothing on disk is rewritten.
const FLAG_ACROSS: u8 = 0b0100_0000;

/// Bytes an expiry instant occupies when a version carries one.
const EXPIRES_LEN: usize = 8;

/// Room a version's provenance takes: the transaction id, its kind byte and
/// the widest reach.
const PROVENANCE_CAPACITY: usize = TRANSACTION_ID_LEN.saturating_add(18);

/// Bytes of header that precede every payload.
const HEADER_LEN: usize = 2;

/// Bytes an epoch occupies when a log record carries one.
const EPOCH_LEN: usize = 8;

/// Bytes a writer's commit order occupies when a log record carries one.
const ORDER_LEN: usize = 8;

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
    /// The millisecond this version stops being answered at, when it has one.
    expires: Option<u64>,
    /// The transaction across leaders this version was resolved from, while
    /// its record stands (ADR-0112 D5).
    provenance: Option<Provenance>,
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
            expires: None,
            provenance: None,
        }
    }

    /// A version written by a node carrying what it had seen.
    #[must_use]
    pub const fn stamped(stamp: CausalStamp, value: RecordValue) -> Self {
        Self {
            value,
            stamp,
            expires: None,
            provenance: None,
        }
    }

    /// This version, as resolved from a transaction across leaders.
    #[must_use]
    pub fn from_transaction(mut self, provenance: Provenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// The transaction across leaders this version came from, if any.
    #[must_use]
    pub const fn provenance(&self) -> Option<&Provenance> {
        self.provenance.as_ref()
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
/// One splitter for the same reason [`log_record::split_epoch`] gives: two readings of one
/// byte string is a thing that can come to disagree with itself, and here the
/// disagreement would be about whether two writes saw each other.
fn split_stamp(bytes: &[u8]) -> Result<(CausalStamp, u8, &[u8])> {
    let (flags, payload) = split_header(
        bytes,
        FLAG_TOMBSTONE | FLAG_STAMP | FLAG_EXPIRES | FLAG_ACROSS,
    )?;
    if flags & FLAG_STAMP == 0 {
        return Ok((CausalStamp::new(), flags, payload));
    }
    let raw: [u8; STAMP_COUNT_LEN] = payload
        .get(..STAMP_COUNT_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(STAMP_COUNT_LEN),
        })?;
    // A count wider than this target's `usize` cannot describe bytes that are
    // here, so it saturates and the length check below reports it truncated.
    let count = usize::try_from(u32::from_be_bytes(raw)).unwrap_or(usize::MAX);
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
    for entry in body.as_chunks::<STAMP_ENTRY_LEN>().0 {
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
        flags,
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
        // A deletion never expires: it is already the absence an expiry would
        // produce, so the instant is dropped rather than written beside it.
        let expires = self.expires.filter(|_| !self.value.is_tombstone());
        if expires.is_some() {
            flags |= FLAG_EXPIRES;
        }
        let stamp_len = if entries.is_empty() {
            0
        } else {
            STAMP_COUNT_LEN.saturating_add(entries.len().saturating_mul(STAMP_ENTRY_LEN))
        };
        let provenance = self.provenance.as_ref().map(|provenance| {
            let mut writer = KeyWriter::with_capacity(PROVENANCE_CAPACITY);
            across::put_provenance(&mut writer, provenance);
            writer.finish()
        });
        if provenance.is_some() {
            flags |= FLAG_ACROSS;
        }
        let payload = self.value.payload();
        let expires_len = if expires.is_some() { EXPIRES_LEN } else { 0 };
        let mut buffer = with_header(
            flags,
            stamp_len
                .saturating_add(expires_len)
                .saturating_add(provenance.as_ref().map_or(0, Vec::len))
                .saturating_add(payload.len()),
        );
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
        if let Some(at) = expires {
            buffer.extend_from_slice(&at.to_be_bytes());
        }
        // After the expiry and before the payload, the order every other
        // optional field keeps: fixed-width prefixes first, then the body.
        if let Some(provenance) = &provenance {
            buffer.extend_from_slice(provenance);
        }
        buffer.extend_from_slice(payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (stamp, flags, rest) = split_stamp(bytes)?;
        let tombstone = flags & FLAG_TOMBSTONE != 0;
        let (expires, rest) = if flags & FLAG_EXPIRES == 0 {
            (None, rest)
        } else {
            if tombstone {
                // No writer puts an instant on a deletion, so one that carries it
                // was written by something this build does not understand.
                return Err(Error::ReservedFlags { flags });
            }
            let (expires, rest) = expiry::split(bytes.len(), rest)?;
            (Some(expires), rest)
        };
        let (provenance, payload) = if flags & FLAG_ACROSS == 0 {
            (None, rest)
        } else {
            let mut reader = KeyReader::new(crate::kind::KeyKind::Record, rest);
            let provenance = across::take_provenance(&mut reader)?;
            (
                Some(provenance),
                rest.get(reader.position()..).unwrap_or_default(),
            )
        };
        let value = if !tombstone {
            RecordValue::Present(payload.to_vec())
        } else if payload.is_empty() {
            RecordValue::Tombstone
        } else {
            return Err(Error::TombstoneWithPayload { len: payload.len() });
        };
        Ok(Self {
            value,
            stamp,
            expires,
            provenance,
        })
    }
}

mod across;
mod expiry;

mod format;
mod log_record;
#[cfg(test)]
mod tests;
