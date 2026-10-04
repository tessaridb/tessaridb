//! The three records of a transaction across leaders, written through the one
//! commit path every write takes (ADR-0112).
//!
//! Each is a commit like any other — placed, fenced on its range's leadership,
//! numbered in its range's log, waited on for a majority by the caller — and
//! differs only in what the record says it is. Reusing the path is the point:
//! a prepare or a decision written past the fence would be a write the epoch
//! does not order.

mod resolving;

use tessari_encoding::{
    Across, LogId, LogKey, LogRecord, Part, Participant, Provenance, RecordValue, StoreKey,
    StoreValue, TransactionId, TransactionRecord,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{Reach, Sequence};

use super::{Committed, RecordAddress, Transaction};
use crate::error::{Error, Result};

/// One home a transaction across leaders writes — the database, or the shard
/// of a split table, an ordinary commit of these records would be filed in —
/// and who writes it.
///
/// By home and not by leader: a participant's prepare must land in the log
/// its records' ordinary commits land in, or the walk after `seen` (ADR-0112
/// D3a) would read a log nobody writes them into.
#[derive(Debug, Clone, PartialEq)]
pub struct AcrossPart {
    /// The home.
    pub home: Reach,
    /// The node leading it, or `None` when this node may write it itself.
    pub leader: Option<[u8; tessari_encoding::NODE_ID_LEN]>,
    /// This node's applied position of the home's log, read before the
    /// conflict check on this node's copy.
    pub seen: Sequence,
    /// The writes that fall in it, as a commit would carry them.
    pub writes: Vec<tessari_encoding::Mutation>,
}

/// What this commit is, for a transaction across leaders.
#[derive(Debug, Clone)]
pub(super) struct Work {
    pub(super) across: Across,
    /// The range holding the transaction's record, which every version this
    /// commit writes names.
    coordinator: Reach,
    /// For a prepare: the participant log's position the transaction's node
    /// had applied (ADR-0112 D3a).
    seen: Option<Sequence>,
    /// For a committed resolution: every participant and where its prepare
    /// landed, which each resolved version carries (D6a).
    participants: Vec<Participant>,
}

impl Transaction<'_> {
    /// Prepare this transaction's buffered writes as intents of `transaction`.
    ///
    /// Every check a commit runs, plus the second half of the conflict check
    /// (D3a): nothing in this range's log after `seen` may have written one of
    /// these records. The answer is the position the intents landed at — the
    /// caller waits for a majority to hold it before answering *prepared*.
    ///
    /// # Errors
    ///
    /// Whatever a commit returns, [`Error::Conflict`] for a record written
    /// after `seen` or under a standing intent, and [`Error::AcrossReadTooOld`]
    /// when this log no longer reaches back to `seen`.
    pub fn prepare_across(
        mut self,
        transaction: TransactionId,
        coordinator: Reach,
        seen: Sequence,
    ) -> Result<Committed> {
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Prepare { coordinator },
            },
            coordinator,
            seen: Some(seen),
            participants: Vec::new(),
        });
        self.commit_placed()
    }

    /// Write a change of `transaction`'s record, in the coordinator's range.
    ///
    /// The change is checked against the record as it stands (`crate::intents`):
    /// a first record must be `PENDING`, and a decided one never changes.
    ///
    /// # Errors
    ///
    /// Whatever a commit returns, and [`Error::AcrossDecided`] for a change the
    /// record's present state refuses.
    pub fn decide_across(
        mut self,
        transaction: TransactionId,
        decided: TransactionRecord,
    ) -> Result<Committed> {
        let coordinator = decided
            .participants
            .first()
            .map(|participant| participant.range)
            .ok_or(Error::AcrossMalformed {
                part: "decide",
                problem: "a record that names no participant",
            })?;
        self.writes.clear();
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Decide(decided),
            },
            coordinator,
            seen: None,
            participants: Vec::new(),
        });
        self.commit_placed()
    }

    /// Bar `transaction`'s part in `range` for good, its prepare not having
    /// landed here — status recovery's step before it aborts a `STAGING`
    /// record (ADR-0112 D14c). Committed in `range`'s own log; the caller
    /// waits for a majority to hold it before answering *barred*.
    ///
    /// # Errors
    ///
    /// Whatever a commit returns, and [`Error::AcrossDecided`] (`prepared`)
    /// when the part has landed here, which no bar can undo.
    pub fn prevent_across(mut self, transaction: TransactionId, range: Reach) -> Result<Committed> {
        self.writes.clear();
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Prevent { range },
            },
            coordinator: range,
            seen: None,
            participants: Vec::new(),
        });
        self.commit_placed()
    }

    /// Begin `transaction` in the coordinator's range: its record written
    /// `STAGING` and this transaction's buffered writes — the coordinator
    /// range's own part — held as intents, in one commit (ADR-0112 D13a,
    /// D14a).
    ///
    /// The record is written only where none stands, so a participant that
    /// found it absent and aborted it while this was on its way wins (D7). The
    /// writes meet every check a prepare's do, `seen` included (D3a).
    ///
    /// # Errors
    ///
    /// Whatever [`Self::prepare_across`] returns, and [`Error::AcrossDecided`]
    /// when the record already stands.
    pub fn begin_across(
        mut self,
        transaction: TransactionId,
        begun: TransactionRecord,
        seen: Sequence,
    ) -> Result<Committed> {
        let coordinator = coordinator_of(&begun, "begin")?;
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Begin(begun),
            },
            coordinator,
            seen: Some(seen),
            participants: Vec::new(),
        });
        self.commit_placed()
    }

    /// Conclude `transaction` in the coordinator's range: its record decided
    /// and the intents it holds there on `records` resolved as the record
    /// says, in one commit (ADR-0112 D13b). No records means every intent of
    /// the transaction this node holds.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::decide_across`] returns, and [`Error::AcrossMalformed`]
    /// for a committed record missing a participant's prepare position.
    pub fn conclude_across(
        mut self,
        transaction: TransactionId,
        decided: TransactionRecord,
        records: &[RecordAddress],
    ) -> Result<Committed> {
        let coordinator = coordinator_of(&decided, "conclude")?;
        let committed = decided.decision == tessari_encoding::Decision::Committed;
        if committed
            && decided
                .participants
                .iter()
                .any(|participant| participant.prepared_at.is_none())
        {
            return Err(Error::AcrossMalformed {
                part: "conclude",
                problem: "a committed record that does not say where every prepare landed",
            });
        }
        self.writes.clear();
        self.buffer_resolutions(transaction, committed, records)?;
        let participants = if committed {
            decided.participants.clone()
        } else {
            Vec::new()
        };
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Conclude(decided),
            },
            coordinator,
            seen: None,
            participants,
        });
        self.commit_placed()
    }

    /// Forget `transaction`'s decided record, in the coordinator's range, once
    /// every participant's resolution is held by a majority (ADR-0112 D12).
    ///
    /// # Errors
    ///
    /// Whatever a commit returns, and [`Error::AcrossMalformed`] for a record
    /// that has not decided.
    pub fn forget_across(
        mut self,
        transaction: TransactionId,
        coordinator: Reach,
    ) -> Result<Committed> {
        self.writes.clear();
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Forget { coordinator },
            },
            coordinator,
            seen: None,
            participants: Vec::new(),
        });
        self.commit_placed()
    }
}

/// The range a record names first, which holds it.
fn coordinator_of(record: &TransactionRecord, part: &'static str) -> Result<Reach> {
    record
        .participants
        .first()
        .map(|participant| participant.range)
        .ok_or(Error::AcrossMalformed {
            part,
            problem: "a record that names no participant",
        })
}

#[cfg(test)]
mod tests;
