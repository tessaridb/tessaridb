//! What the log says about a transaction across leaders (ADR-0112).
//!
//! A transaction whose writes fall in ranges led by different nodes commits in
//! three kinds of record, each in one range's log: a participant's **prepare**
//! (its writes as intents), the coordinator's **decision** (the transaction
//! record), and a participant's **resolution** (intents become versions, or are
//! dropped). The section here is what tells them apart and names the
//! transaction; the mutations, where a record has any, follow it as in every
//! other log record.
//!
//! A version resolved from such a transaction carries its [`Provenance`] for as
//! long as the transaction's record stands, because a reader deciding whether
//! to see the transaction (ADR-0112 D6) has to know that a plain-looking value
//! is part of one.

use tessari_types::{Reach, Sequence};

use tessari_kv::Value;

use super::{StoreValue, split_header, with_header};
use crate::error::{Error, Result};
use crate::keys::{put_reach, take_reach};
use crate::order::{KeyReader, KeyWriter};

/// Bytes a transaction id occupies.
pub const TRANSACTION_ID_LEN: usize = 16;

/// The name of one transaction across leaders, chosen by its coordinator.
///
/// Sixteen bytes chosen at random by the coordinator and written into every
/// record the transaction produces, so no node ever has to agree with another
/// about the next one — the same reason a log's writer is a node id rather
/// than an allocated number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId([u8; TRANSACTION_ID_LEN]);

impl TransactionId {
    /// The id the given bytes name.
    #[must_use]
    pub const fn new(bytes: [u8; TRANSACTION_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// The bytes a record carries.
    #[must_use]
    pub const fn bytes(self) -> [u8; TRANSACTION_ID_LEN] {
        self.0
    }
}

/// What the transaction record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decision {
    /// Written before the first prepare is sent; nothing is decided.
    Pending,
    /// Every participant prepared, and the record says so.
    Committed,
    /// A participant refused, or the record's liveness lapsed.
    Aborted,
}

/// One range the transaction writes, as its record names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Participant {
    /// The range — its leader prepares the transaction's writes there.
    pub range: Reach,
    /// Where in that range's log its prepare landed, once known. A reader and
    /// a backup both need it: a copy behind this position has not seen the
    /// transaction's writes in that range (ADR-0112 D6, D9).
    pub prepared_at: Option<Sequence>,
}

/// The transaction record: the one place its outcome is decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionRecord {
    /// The outcome, or that there is none yet.
    pub decision: Decision,
    /// The millisecond after which a `PENDING` record may be aborted by anyone
    /// (ADR-0112 D7). Data rather than a rule, as an expiry is: every node
    /// compares the same instant against its own clock.
    pub deadline: u64,
    /// Every range the transaction writes, in the order the coordinator chose.
    pub participants: Vec<Participant>,
}

/// Which of the three records this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// A participant's writes, held as intents; the coordinator's range is
    /// where the record that decides them lives.
    Prepare {
        /// The range whose log holds the transaction record.
        coordinator: Reach,
    },
    /// A change of the transaction record, written in the coordinator's range.
    Decide(TransactionRecord),
    /// A participant's intents resolved: written as versions when committed,
    /// dropped when not.
    Resolve {
        /// The record's outcome this resolution applies.
        committed: bool,
    },
    /// The decided record deleted, every participant's resolution being held
    /// by a majority (ADR-0112 D12) — written in the coordinator's range.
    Forget {
        /// The range whose log holds the transaction record.
        coordinator: Reach,
    },
    /// The transaction's part in `range` had landed where a snapshot was read
    /// (ADR-0112 D9a). Only a restored state carries it: a log records the
    /// prepare itself.
    Landed {
        /// The participant range whose part had landed.
        range: Reach,
    },
    /// The transaction record written `PENDING` and the coordinator's own
    /// range's writes held as intents, in one record (ADR-0112 D13a): the
    /// record by compare-and-set on *absent*, so a participant that aborted it
    /// first wins.
    Begin(TransactionRecord),
    /// The record decided and the coordinator's own range's intents resolved
    /// as it says, in one record (ADR-0112 D13b).
    Conclude(TransactionRecord),
}

impl Part {
    /// The transaction record this part writes, if it writes one.
    #[must_use]
    pub const fn record(&self) -> Option<&TransactionRecord> {
        match self {
            Self::Decide(record) | Self::Begin(record) | Self::Conclude(record) => Some(record),
            Self::Prepare { .. }
            | Self::Resolve { .. }
            | Self::Forget { .. }
            | Self::Landed { .. } => None,
        }
    }

    /// Whether this part's writes are intents of its transaction.
    #[must_use]
    pub const fn prepares(&self) -> bool {
        matches!(self, Self::Prepare { .. } | Self::Begin(_))
    }

    /// The outcome this part resolves intents to, if it resolves any: `true`
    /// when they become versions, `false` when they are dropped.
    #[must_use]
    pub fn resolution(&self) -> Option<bool> {
        match self {
            Self::Resolve { committed } => Some(*committed),
            Self::Conclude(record) => Some(record.decision == Decision::Committed),
            Self::Prepare { .. }
            | Self::Decide(_)
            | Self::Forget { .. }
            | Self::Landed { .. }
            | Self::Begin(_) => None,
        }
    }
}

/// The section a log record carries when it belongs to a transaction across
/// leaders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Across {
    /// The transaction this record belongs to.
    pub transaction: TransactionId,
    /// What this record does for it.
    pub part: Part,
}

/// Where a version came from when a transaction across leaders wrote it.
///
/// A participant's prepare stores each write as a **provisional** version under
/// the record's own key — an intent — and its resolution replaces it with the
/// final version, which keeps the provenance with `provisional` clear. An
/// intent therefore lives where readers already walk versions, and is never a
/// second keyspace every read would have to consult.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Provenance {
    /// The transaction.
    pub transaction: TransactionId,
    /// Whether this version is an intent, which only the transaction's record
    /// can turn into a value (ADR-0112 D5).
    pub provisional: bool,
    /// The range whose log holds its record.
    pub coordinator: Reach,
    /// Every range the transaction wrote, with where its prepare landed — on a
    /// resolved version, so a reader can tell whether its snapshot holds the
    /// whole transaction without the record (ADR-0112 D6a). Empty on an
    /// intent: its prepare is the first one known, and the record has the rest
    /// once it commits.
    pub participants: Vec<Participant>,
}

const PART_PREPARE: u8 = 1;
const PART_DECIDE: u8 = 2;
const PART_RESOLVE: u8 = 3;
const PART_FORGET: u8 = 4;
const PART_LANDED: u8 = 5;
const PART_BEGIN: u8 = 6;
const PART_CONCLUDE: u8 = 7;

const RESOLVED: u8 = 0;
const PROVISIONAL: u8 = 1;

const DECISION_PENDING: u8 = 0;
const DECISION_COMMITTED: u8 = 1;
const DECISION_ABORTED: u8 = 2;

/// Append an [`Across`] section.
pub(super) fn put(writer: &mut KeyWriter, across: &Across) {
    writer.put_fixed(&across.transaction.bytes());
    match &across.part {
        Part::Prepare { coordinator } => {
            writer.put_u8(PART_PREPARE);
            put_reach(writer, *coordinator);
        }
        Part::Decide(record) => {
            writer.put_u8(PART_DECIDE);
            put_record(writer, record);
        }
        Part::Resolve { committed } => {
            writer.put_u8(PART_RESOLVE).put_u8(u8::from(*committed));
        }
        Part::Forget { coordinator } => {
            writer.put_u8(PART_FORGET);
            put_reach(writer, *coordinator);
        }
        Part::Landed { range } => {
            writer.put_u8(PART_LANDED);
            put_reach(writer, *range);
        }
        Part::Begin(record) => {
            writer.put_u8(PART_BEGIN);
            put_record(writer, record);
        }
        Part::Conclude(record) => {
            writer.put_u8(PART_CONCLUDE);
            put_record(writer, record);
        }
    }
}

/// Read an [`Across`] section written by [`put`].
///
/// # Errors
///
/// Returns [`Error::UnknownAcross`] for a part, decision or flag byte this
/// build does not know, and whatever the reader returns when the bytes are
/// short.
pub(super) fn take(reader: &mut KeyReader<'_>) -> Result<Across> {
    let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
    let part = match reader.take_u8()? {
        PART_PREPARE => Part::Prepare {
            coordinator: take_reach(reader)?,
        },
        PART_DECIDE => Part::Decide(take_record(reader)?),
        PART_RESOLVE => Part::Resolve {
            committed: match reader.take_u8()? {
                0 => false,
                1 => true,
                found => return Err(unknown("resolution", found)),
            },
        },
        PART_FORGET => Part::Forget {
            coordinator: take_reach(reader)?,
        },
        PART_LANDED => Part::Landed {
            range: take_reach(reader)?,
        },
        PART_BEGIN => Part::Begin(take_record(reader)?),
        PART_CONCLUDE => Part::Conclude(take_record(reader)?),
        found => return Err(unknown("part", found)),
    };
    Ok(Across { transaction, part })
}

/// Append a [`TransactionRecord`]: as a `Decide`, `Begin` or `Conclude` part,
/// and as the value the record's own key holds.
fn put_record(writer: &mut KeyWriter, record: &TransactionRecord) {
    writer.put_u8(match record.decision {
        Decision::Pending => DECISION_PENDING,
        Decision::Committed => DECISION_COMMITTED,
        Decision::Aborted => DECISION_ABORTED,
    });
    writer.put_u64(record.deadline);
    // A count here, unlike in front of a log record's mutations: in a part
    // carrying a record the participants are followed by the mutations, so they need an end.
    put_participants(writer, &record.participants);
}

/// Append a count of participants and each one: its range, then its prepare
/// position when known.
fn put_participants(writer: &mut KeyWriter, participants: &[Participant]) {
    writer.put_u32(u32::try_from(participants.len()).unwrap_or(u32::MAX));
    for participant in participants {
        put_reach(writer, participant.range);
        match participant.prepared_at {
            Some(at) => writer.put_u8(1).put_u64(at.get()),
            None => writer.put_u8(0),
        };
    }
}

/// Read participants written by [`put_participants`].
fn take_participants(reader: &mut KeyReader<'_>) -> Result<Vec<Participant>> {
    let count = reader.take_u32()?;
    let mut participants = Vec::new();
    for _ in 0..count {
        let range = take_reach(reader)?;
        let prepared_at = match reader.take_u8()? {
            0 => None,
            1 => Some(Sequence::new(reader.take_u64()?)),
            found => return Err(unknown("prepare position", found)),
        };
        participants.push(Participant { range, prepared_at });
    }
    Ok(participants)
}

/// Read a [`TransactionRecord`] written by [`put_record`].
fn take_record(reader: &mut KeyReader<'_>) -> Result<TransactionRecord> {
    let decision = match reader.take_u8()? {
        DECISION_PENDING => Decision::Pending,
        DECISION_COMMITTED => Decision::Committed,
        DECISION_ABORTED => Decision::Aborted,
        found => return Err(unknown("decision", found)),
    };
    let deadline = reader.take_u64()?;
    let participants = take_participants(reader)?;
    Ok(TransactionRecord {
        decision,
        deadline,
        participants,
    })
}

impl StoreValue for TransactionRecord {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::with_capacity(32);
        put_record(&mut writer, self);
        let payload = writer.finish();
        let mut buffer = with_header(0, payload.len());
        buffer.extend_from_slice(&payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(crate::kind::KeyKind::TransactionRecord, payload);
        let record = take_record(&mut reader)?;
        reader.finish()?;
        Ok(record)
    }
}

/// Append a version's [`Provenance`]: the transaction first, as on a log
/// record, then whether it is an intent, then the coordinator's range, and on a
/// resolved version the participants — an intent carries none, so an intent
/// costs what it did.
pub(super) fn put_provenance(writer: &mut KeyWriter, provenance: &Provenance) {
    writer.put_fixed(&provenance.transaction.bytes());
    writer.put_u8(if provenance.provisional {
        PROVISIONAL
    } else {
        RESOLVED
    });
    put_reach(writer, provenance.coordinator);
    if !provenance.provisional {
        put_participants(writer, &provenance.participants);
    }
}

/// Read a version's [`Provenance`] written by [`put_provenance`].
///
/// # Errors
///
/// Whatever the reader returns when the bytes are short or the reach unknown.
pub(super) fn take_provenance(reader: &mut KeyReader<'_>) -> Result<Provenance> {
    let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
    let provisional = match reader.take_u8()? {
        RESOLVED => false,
        PROVISIONAL => true,
        found => return Err(unknown("provenance", found)),
    };
    let coordinator = take_reach(reader)?;
    let participants = if provisional {
        Vec::new()
    } else {
        take_participants(reader)?
    };
    Ok(Provenance {
        transaction,
        provisional,
        coordinator,
        participants,
    })
}

const fn unknown(what: &'static str, found: u8) -> Error {
    Error::UnknownAcross { what, found }
}

#[cfg(test)]
mod tests;
