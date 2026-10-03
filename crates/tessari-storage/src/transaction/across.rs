//! The three records of a transaction across leaders, written through the one
//! commit path every write takes (ADR-0112).
//!
//! Each is a commit like any other — placed, fenced on its range's leadership,
//! numbered in its range's log, waited on for a majority by the caller — and
//! differs only in what the record says it is. Reusing the path is the point:
//! a prepare or a decision written past the fence would be a write the epoch
//! does not order.

use tessari_encoding::{
    Across, LogId, LogKey, LogRecord, Part, Provenance, RecordValue, StoreKey, StoreValue,
    TransactionId, TransactionRecord,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{Reach, Sequence};

use super::{Committed, RecordAddress, Transaction};
use crate::error::{Error, Result};

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
        });
        self.commit_placed()
    }

    /// Resolve `transaction`'s intents on `records` as the record decided:
    /// committed, their values become versions; aborted, they are dropped.
    ///
    /// Idempotent: a record whose intent is already gone is passed over, and
    /// `None` answers a call that found nothing left to resolve.
    ///
    /// # Errors
    ///
    /// Whatever a commit returns.
    pub fn resolve_across(
        mut self,
        transaction: TransactionId,
        coordinator: Reach,
        committed: bool,
        records: &[RecordAddress],
    ) -> Result<Option<Committed>> {
        self.writes.clear();
        for address in records {
            let Some(intent) = self.intent_of(address, transaction)? else {
                continue;
            };
            let value = if committed {
                intent
            } else {
                // Named, never written: an aborted resolution drops the intent.
                RecordValue::Tombstone
            };
            self.writes.insert(address.clone(), value);
        }
        if self.writes.is_empty() {
            return Ok(None);
        }
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Resolve { committed },
            },
            coordinator,
            seen: None,
        });
        self.commit_placed().map(Some)
    }

    /// The value `transaction` holds as an intent on `address`, if it does.
    fn intent_of(
        &self,
        address: &RecordAddress,
        transaction: TransactionId,
    ) -> Result<Option<RecordValue>> {
        let Some((_, provenance, value)) = self.newest_stored_value(address)? else {
            return Ok(None);
        };
        Ok(provenance
            .filter(|provenance| provenance.provisional && provenance.transaction == transaction)
            .map(|_| value))
    }

    /// Mark a record this commit is about to write with what it is, and every
    /// version in it with where it came from.
    pub(super) fn mark_across(&self, record: LogRecord) -> LogRecord {
        let Some(work) = &self.across else {
            return record;
        };
        let provisional = match work.across.part {
            Part::Prepare { .. } => true,
            Part::Resolve { committed: true } => false,
            // A decision has no versions, and an aborted resolution's are
            // never written.
            Part::Decide(_) | Part::Resolve { committed: false } => {
                return record.across(work.across.clone());
            }
        };
        let provenance = Provenance {
            transaction: work.across.transaction,
            provisional,
            coordinator: work.coordinator,
        };
        record
            .with_provenance(provenance)
            .across(work.across.clone())
    }

    /// Whether this commit writes a record of a transaction across leaders,
    /// which a commit with no buffered writes still does when it is a decision.
    pub(super) const fn is_across(&self) -> bool {
        self.across.is_some()
    }

    /// The coordinator's range when this commit is a decision, which writes no
    /// record and must still be admitted into that range.
    pub(super) fn decision_range(&self) -> Option<Reach> {
        self.across
            .as_ref()
            .filter(|work| matches!(work.across.part, Part::Decide(_)))
            .map(|work| work.coordinator)
    }

    /// Whether `provenance` is an intent this commit itself resolves, which
    /// the conflict check must not refuse it for.
    pub(super) fn resolves(&self, provenance: Option<Provenance>) -> bool {
        match (&self.across, provenance) {
            (Some(work), Some(provenance)) => {
                matches!(work.across.part, Part::Resolve { .. })
                    && provenance.provisional
                    && provenance.transaction == work.across.transaction
            }
            _ => false,
        }
    }

    /// The second half of a prepare's conflict check (ADR-0112 D3a): refuse
    /// if any record of `log` after the position the transaction had seen
    /// writes one of these records.
    pub(super) fn written_since_seen(&self, log: LogId) -> Result<()> {
        let Some(seen) = self.across.as_ref().and_then(|work| work.seen) else {
            return Ok(());
        };
        let start = self.store.log_start(log)?;
        let next = Sequence::new(seen.get().saturating_add(1));
        if start > next {
            return Err(Error::AcrossReadTooOld { seen, start });
        }
        let prefix = LogKey::prefix_for(log);
        let range = KeyRange::from_bounds(
            std::ops::Bound::Included(LogKey::new(log, next).encode()),
            KeyRange::prefix(&prefix).end().clone(),
        );
        let entries = self.store.backend().scan(&ScanRequest {
            keyspace: LogKey::keyspace(),
            range,
            direction: ScanDirection::Forward,
            limit: None,
        })?;
        for (key, value) in entries {
            let record = LogRecord::decode(value.as_slice())?;
            for mutation in record.mutations() {
                let address = RecordAddress::new(
                    mutation.namespace,
                    mutation.database,
                    mutation.table,
                    mutation.id.clone(),
                );
                if self.writes.contains_key(&address) {
                    return Err(Error::Conflict {
                        id: address.id,
                        snapshot: seen,
                        committed: LogKey::decode(key.as_slice())?.sequence,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
