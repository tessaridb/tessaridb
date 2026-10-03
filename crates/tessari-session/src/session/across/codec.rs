//! How a record of a transaction across leaders travels between nodes.
//!
//! As a log record — the section that says which record it is, and the
//! mutations — behind the one number the log record has no field for. The log
//! record's codec is already the format this store keeps forever, checks on
//! every apply and tests byte for byte; a second encoding of the same writes
//! would be a second thing to keep in step with it.
//!
//! ```text
//! <kind:u8> <seen:u64> <log record>
//! ```
//!
//! The kind says which ask it is, because a settle is not a record the log
//! keeps and so has no section of its own: it travels as a prepare's section
//! with no writes. `seen` is a prepare's (ADR-0112 D3a) and zero otherwise. An
//! aborted resolution names its records with tombstones, which it never writes.

use tessari_encoding::{
    Across, Decision, LogRecord, Mutation, Part, RecordValue, StampedValue, StoreValue,
};
use tessari_storage::RecordAddress;
use tessari_types::Sequence;

use super::{AcrossAnswer, AcrossAsk};

const ASK_PREPARE: u8 = 1;
const ASK_DECIDE: u8 = 2;
const ASK_RESOLVE: u8 = 3;
const ASK_SETTLE: u8 = 4;

const PREPARED: u8 = 1;
const DECIDED: u8 = 2;
const RESOLVED: u8 = 3;
const NOTHING_LEFT: u8 = 4;
const OUTCOME_PENDING: u8 = 5;
const OUTCOME_COMMITTED: u8 = 6;
const OUTCOME_ABORTED: u8 = 7;

impl AcrossAsk {
    /// The bytes a peer frame carries.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (kind, seen, record) = match self {
            Self::Prepare {
                transaction,
                coordinator,
                seen,
                writes,
            } => (
                ASK_PREPARE,
                *seen,
                LogRecord::new(writes.clone()).across(Across {
                    transaction: *transaction,
                    part: Part::Prepare {
                        coordinator: *coordinator,
                    },
                }),
            ),
            Self::Decide {
                transaction,
                record,
            } => (
                ASK_DECIDE,
                Sequence::ZERO,
                LogRecord::new(Vec::new()).across(Across {
                    transaction: *transaction,
                    part: Part::Decide(record.clone()),
                }),
            ),
            Self::Settle {
                transaction,
                coordinator,
            } => (
                ASK_SETTLE,
                Sequence::ZERO,
                LogRecord::new(Vec::new()).across(Across {
                    transaction: *transaction,
                    part: Part::Prepare {
                        coordinator: *coordinator,
                    },
                }),
            ),
            Self::Resolve {
                transaction,
                committed,
                records,
            } => (
                ASK_RESOLVE,
                Sequence::ZERO,
                LogRecord::new(records.iter().map(named).collect()).across(Across {
                    transaction: *transaction,
                    part: Part::Resolve {
                        committed: *committed,
                    },
                }),
            ),
        };
        let encoded = record.encode();
        let mut bytes = Vec::with_capacity(encoded.as_slice().len().saturating_add(9));
        bytes.push(kind);
        bytes.extend_from_slice(&seen.get().to_be_bytes());
        bytes.extend_from_slice(encoded.as_slice());
        bytes
    }

    /// Read what [`Self::encode`] wrote.
    ///
    /// # Errors
    ///
    /// The codec's own error for bytes that are not a log record, and
    /// `None`-shaped refusals as text for one that is not a record of a
    /// transaction across leaders.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (kind, rest) = bytes
            .split_first()
            .ok_or_else(|| "an empty cross-leader request".to_owned())?;
        let (seen, record) = rest
            .split_first_chunk::<8>()
            .ok_or_else(|| "a cross-leader request shorter than its position".to_owned())?;
        let seen = Sequence::new(u64::from_be_bytes(*seen));
        let record = LogRecord::decode(record).map_err(|why| why.to_string())?;
        let across = record
            .part_of()
            .cloned()
            .ok_or_else(|| "a cross-leader request that names no transaction".to_owned())?;
        Ok(match (*kind, across.part) {
            (ASK_SETTLE, Part::Prepare { coordinator }) if record.mutations().is_empty() => {
                Self::Settle {
                    transaction: across.transaction,
                    coordinator,
                }
            }
            (ASK_PREPARE, Part::Prepare { coordinator }) => Self::Prepare {
                transaction: across.transaction,
                coordinator,
                seen,
                writes: record.mutations().to_vec(),
            },
            (ASK_DECIDE, Part::Decide(decided)) => Self::Decide {
                transaction: across.transaction,
                record: decided,
            },
            (ASK_RESOLVE, Part::Resolve { committed }) => Self::Resolve {
                transaction: across.transaction,
                committed,
                records: record
                    .mutations()
                    .iter()
                    .map(|mutation| {
                        RecordAddress::new(
                            mutation.namespace,
                            mutation.database,
                            mutation.table,
                            mutation.id.clone(),
                        )
                    })
                    .collect(),
            },
            (kind, _) => {
                return Err(format!(
                    "a cross-leader request of kind {kind} carrying another kind's section"
                ));
            }
        })
    }
}

/// A record named by a resolution: its address, and a tombstone nobody writes.
fn named(address: &RecordAddress) -> Mutation {
    Mutation {
        namespace: address.namespace,
        database: address.database,
        table: address.table,
        id: address.id.clone(),
        shard: None,
        value: StampedValue::new(RecordValue::Tombstone),
    }
}

impl AcrossAnswer {
    /// The bytes a peer frame carries: a tag and a position.
    #[must_use]
    pub fn encode(&self) -> [u8; 9] {
        let (tag, at) = match self {
            Self::Prepared(at) => (PREPARED, at.get()),
            Self::Decided(at) => (DECIDED, at.get()),
            Self::Resolved(Some(at)) => (RESOLVED, at.get()),
            Self::Resolved(None) => (NOTHING_LEFT, 0),
            Self::Outcome(Decision::Pending) => (OUTCOME_PENDING, 0),
            Self::Outcome(Decision::Committed) => (OUTCOME_COMMITTED, 0),
            Self::Outcome(Decision::Aborted) => (OUTCOME_ABORTED, 0),
        };
        let mut bytes = [0; 9];
        bytes[0] = tag;
        bytes[1..].copy_from_slice(&at.to_be_bytes());
        bytes
    }

    /// Read what [`Self::encode`] wrote.
    ///
    /// # Errors
    ///
    /// A refusal in words for bytes that are not an answer.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (tag, at) = bytes
            .split_first()
            .filter(|(_, at)| at.len() == 8)
            .ok_or_else(|| "a cross-leader answer of the wrong length".to_owned())?;
        let mut position = [0; 8];
        position.copy_from_slice(at);
        let at = Sequence::new(u64::from_be_bytes(position));
        match *tag {
            PREPARED => Ok(Self::Prepared(at)),
            DECIDED => Ok(Self::Decided(at)),
            RESOLVED => Ok(Self::Resolved(Some(at))),
            NOTHING_LEFT => Ok(Self::Resolved(None)),
            OUTCOME_PENDING => Ok(Self::Outcome(Decision::Pending)),
            OUTCOME_COMMITTED => Ok(Self::Outcome(Decision::Committed)),
            OUTCOME_ABORTED => Ok(Self::Outcome(Decision::Aborted)),
            found => Err(format!("a cross-leader answer of unknown kind {found}")),
        }
    }
}

#[cfg(test)]
mod tests;
