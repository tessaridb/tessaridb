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
//! with no writes. `seen` is a prepare's (ADR-0112 D3a) and zero otherwise. A
//! resolution travels as a decision's section — the outcome and the
//! participants with their prepare positions, which its versions carry (D6a)
//! — and names its records with tombstones, which it never writes.
//!
//! A begin travels as a prepare does, its section carrying the `PENDING`
//! record; a conclusion as a resolution does, its section carrying the
//! decided record (ADR-0112 D13a, D13b).
//!
//! Status recovery's question travels as the bar it may write, its kind saying
//! whether to write it (ADR-0112 D14c).
//!
//! An answer is a tag and a position, except an outcome, which is the tag and
//! the record as it stands.

use tessari_encoding::{
    Across, Decision, LogRecord, Mutation, Part, RecordValue, StampedValue, StoreValue,
    TransactionRecord,
};
use tessari_storage::RecordAddress;
use tessari_types::Sequence;

use super::{AcrossAnswer, AcrossAsk};

const ASK_PREPARE: u8 = 1;
const ASK_DECIDE: u8 = 2;
const ASK_RESOLVE: u8 = 3;
const ASK_SETTLE: u8 = 4;
const ASK_HOLDS: u8 = 5;
const ASK_FORGET: u8 = 6;
const ASK_LOOKUP: u8 = 7;
const ASK_BEGIN: u8 = 8;
const ASK_CONCLUDE: u8 = 9;
const ASK_BAR: u8 = 10;
const ASK_LANDED: u8 = 11;

const PREPARED: u8 = 1;
const DECIDED: u8 = 2;
const RESOLVED: u8 = 3;
const NOTHING_LEFT: u8 = 4;
const OUTCOME: u8 = 5;
const HOLDING: u8 = 6;
const FORGOTTEN: u8 = 7;
const LANDED: u8 = 8;
const NOT_LANDED: u8 = 9;

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
            Self::Lookup {
                transaction,
                coordinator,
            } => (
                ASK_LOOKUP,
                Sequence::ZERO,
                LogRecord::new(Vec::new()).across(Across {
                    transaction: *transaction,
                    part: Part::Prepare {
                        coordinator: *coordinator,
                    },
                }),
            ),
            Self::Holds { transaction, range } => (
                ASK_HOLDS,
                Sequence::ZERO,
                // The range rides where a prepare names its coordinator: the
                // one reach the section carries.
                LogRecord::new(Vec::new()).across(Across {
                    transaction: *transaction,
                    part: Part::Prepare {
                        coordinator: *range,
                    },
                }),
            ),
            Self::Forget {
                transaction,
                coordinator,
            } => (
                ASK_FORGET,
                Sequence::ZERO,
                LogRecord::new(Vec::new()).across(Across {
                    transaction: *transaction,
                    part: Part::Forget {
                        coordinator: *coordinator,
                    },
                }),
            ),
            Self::Bar {
                transaction,
                range,
                prevent,
            } => (
                if *prevent { ASK_BAR } else { ASK_LANDED },
                Sequence::ZERO,
                LogRecord::new(Vec::new()).across(Across {
                    transaction: *transaction,
                    part: Part::Prevent { range: *range },
                }),
            ),
            Self::Begin {
                transaction,
                record,
                seen,
                writes,
            } => (
                ASK_BEGIN,
                *seen,
                LogRecord::new(writes.clone()).across(Across {
                    transaction: *transaction,
                    part: Part::Begin(record.clone()),
                }),
            ),
            Self::Conclude {
                transaction,
                record,
                records,
            } => (
                ASK_CONCLUDE,
                Sequence::ZERO,
                LogRecord::new(records.iter().map(named).collect()).across(Across {
                    transaction: *transaction,
                    part: Part::Conclude(record.clone()),
                }),
            ),
            Self::Resolve {
                transaction,
                committed,
                records,
                participants,
            } => (
                ASK_RESOLVE,
                Sequence::ZERO,
                LogRecord::new(records.iter().map(named).collect()).across(Across {
                    transaction: *transaction,
                    part: Part::Decide(TransactionRecord {
                        decision: if *committed {
                            Decision::Committed
                        } else {
                            Decision::Aborted
                        },
                        deadline: 0,
                        participants: participants.clone(),
                    }),
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
            (ASK_LOOKUP, Part::Prepare { coordinator }) if record.mutations().is_empty() => {
                Self::Lookup {
                    transaction: across.transaction,
                    coordinator,
                }
            }
            (ASK_HOLDS, Part::Prepare { coordinator }) if record.mutations().is_empty() => {
                Self::Holds {
                    transaction: across.transaction,
                    range: coordinator,
                }
            }
            (kind @ (ASK_BAR | ASK_LANDED), Part::Prevent { range })
                if record.mutations().is_empty() =>
            {
                Self::Bar {
                    transaction: across.transaction,
                    range,
                    prevent: kind == ASK_BAR,
                }
            }
            (ASK_FORGET, Part::Forget { coordinator }) if record.mutations().is_empty() => {
                Self::Forget {
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
            (ASK_BEGIN, Part::Begin(begun)) => Self::Begin {
                transaction: across.transaction,
                record: begun,
                seen,
                writes: record.mutations().to_vec(),
            },
            (ASK_CONCLUDE, Part::Conclude(decided)) => Self::Conclude {
                transaction: across.transaction,
                record: decided,
                records: addresses(&record),
            },
            (ASK_RESOLVE, Part::Decide(outcome)) => Self::Resolve {
                transaction: across.transaction,
                committed: outcome.decision == Decision::Committed,
                participants: outcome.participants,
                records: addresses(&record),
            },
            (kind, _) => {
                return Err(format!(
                    "a cross-leader request of kind {kind} carrying another kind's section"
                ));
            }
        })
    }
}

/// The records a resolution names, as their addresses.
fn addresses(record: &LogRecord) -> Vec<RecordAddress> {
    record
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
        .collect()
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
    /// The bytes a peer frame carries: a tag and a position, or a tag and the
    /// record for an outcome.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (tag, at) = match self {
            Self::Prepared(at) => (PREPARED, at.get()),
            Self::Decided(at) => (DECIDED, at.get()),
            Self::Resolved(Some(at)) => (RESOLVED, at.get()),
            Self::Resolved(None) => (NOTHING_LEFT, 0),
            Self::Holding(holds) => (HOLDING, u64::from(*holds)),
            Self::Forgotten(at) => (FORGOTTEN, at.get()),
            Self::Landed(Some(at)) => (LANDED, at.get()),
            Self::Landed(None) => (NOT_LANDED, 0),
            Self::Outcome(record) => {
                let encoded = record.encode();
                let mut bytes = Vec::with_capacity(encoded.as_slice().len().saturating_add(1));
                bytes.push(OUTCOME);
                bytes.extend_from_slice(encoded.as_slice());
                return bytes;
            }
        };
        let mut bytes = Vec::with_capacity(9);
        bytes.push(tag);
        bytes.extend_from_slice(&at.to_be_bytes());
        bytes
    }

    /// Read what [`Self::encode`] wrote.
    ///
    /// # Errors
    ///
    /// A refusal in words for bytes that are not an answer.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if let Some((&OUTCOME, record)) = bytes.split_first() {
            return TransactionRecord::decode(record)
                .map(Self::Outcome)
                .map_err(|why| why.to_string());
        }
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
            HOLDING => match at.get() {
                0 => Ok(Self::Holding(false)),
                1 => Ok(Self::Holding(true)),
                found => Err(format!("a cross-leader holding answer of {found}")),
            },
            FORGOTTEN => Ok(Self::Forgotten(at)),
            LANDED => Ok(Self::Landed(Some(at))),
            NOT_LANDED => Ok(Self::Landed(None)),
            found => Err(format!("a cross-leader answer of unknown kind {found}")),
        }
    }
}

#[cfg(test)]
mod tests;
