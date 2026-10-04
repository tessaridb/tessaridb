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
pub use stamped::StampedValue;

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

/// Every defined flag bit and what it names, for the format surface.
pub(crate) const FLAGS: &[(u8, &str)] = &[
    (FLAG_TOMBSTONE, "tombstone"),
    (FLAG_EPOCH, "epoch"),
    (FLAG_STAMP, "stamp"),
    (FLAG_SHARDS, "shards"),
    (FLAG_ORDER, "order"),
    (FLAG_EXPIRES, "expires"),
    (FLAG_ACROSS, "across"),
];

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

mod across;
mod expiry;

mod format;
mod log_record;
mod stamped;
#[cfg(test)]
mod tests;
