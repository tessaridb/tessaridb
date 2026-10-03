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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Provenance {
    /// The transaction.
    pub transaction: TransactionId,
    /// The range whose log holds its record.
    pub coordinator: Reach,
}

const PART_PREPARE: u8 = 1;
const PART_DECIDE: u8 = 2;
const PART_RESOLVE: u8 = 3;

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
            writer.put_u8(match record.decision {
                Decision::Pending => DECISION_PENDING,
                Decision::Committed => DECISION_COMMITTED,
                Decision::Aborted => DECISION_ABORTED,
            });
            writer.put_u64(record.deadline);
            // A count here, unlike in front of the mutations: the participants
            // are followed by the mutations, so they need a stated end.
            writer.put_u32(u32::try_from(record.participants.len()).unwrap_or(u32::MAX));
            for participant in &record.participants {
                put_reach(writer, participant.range);
                match participant.prepared_at {
                    Some(at) => writer.put_u8(1).put_u64(at.get()),
                    None => writer.put_u8(0),
                };
            }
        }
        Part::Resolve { committed } => {
            writer.put_u8(PART_RESOLVE).put_u8(u8::from(*committed));
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
        PART_DECIDE => {
            let decision = match reader.take_u8()? {
                DECISION_PENDING => Decision::Pending,
                DECISION_COMMITTED => Decision::Committed,
                DECISION_ABORTED => Decision::Aborted,
                found => return Err(unknown("decision", found)),
            };
            let deadline = reader.take_u64()?;
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
            Part::Decide(TransactionRecord {
                decision,
                deadline,
                participants,
            })
        }
        PART_RESOLVE => Part::Resolve {
            committed: match reader.take_u8()? {
                0 => false,
                1 => true,
                found => return Err(unknown("resolution", found)),
            },
        },
        found => return Err(unknown("part", found)),
    };
    Ok(Across { transaction, part })
}

/// Append a version's [`Provenance`].
pub(super) fn put_provenance(writer: &mut KeyWriter, provenance: Provenance) {
    writer.put_fixed(&provenance.transaction.bytes());
    put_reach(writer, provenance.coordinator);
}

/// Read a version's [`Provenance`] written by [`put_provenance`].
///
/// # Errors
///
/// Whatever the reader returns when the bytes are short or the reach unknown.
pub(super) fn take_provenance(reader: &mut KeyReader<'_>) -> Result<Provenance> {
    Ok(Provenance {
        transaction: TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?),
        coordinator: take_reach(reader)?,
    })
}

const fn unknown(what: &'static str, found: u8) -> Error {
    Error::UnknownAcross { what, found }
}

#[cfg(test)]
mod tests;
