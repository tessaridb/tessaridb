//! The three records of a transaction across leaders, written through the one
//! commit path every write takes (ADR-0112).
//!
//! Each is a commit like any other — placed, fenced on its range's leadership,
//! numbered in its range's log, waited on for a majority by the caller — and
//! differs only in what the record says it is. Reusing the path is the point:
//! a prepare or a decision written past the fence would be a write the epoch
//! does not order.

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

    /// Resolve `transaction`'s intents on `records` as the record decided:
    /// committed, their values become versions; aborted, they are dropped.
    /// No records means every intent of the transaction this node holds.
    ///
    /// A committed resolution names `participants` as the committed record
    /// does, every prepare position known: each version it writes carries
    /// them, which is how a reader that holds no copy of the record decides
    /// whether its snapshot holds the whole transaction (D6a).
    ///
    /// Idempotent: a record whose intent is already gone is passed over, and
    /// `None` answers a call that found nothing left to resolve.
    ///
    /// # Errors
    ///
    /// Whatever a commit returns, and [`Error::AcrossMalformed`] for a
    /// committed resolution missing a participant's prepare position.
    pub fn resolve_across(
        mut self,
        transaction: TransactionId,
        committed: bool,
        records: &[RecordAddress],
        participants: &[Participant],
    ) -> Result<Option<Committed>> {
        if committed
            && (participants.is_empty()
                || participants
                    .iter()
                    .any(|participant| participant.prepared_at.is_none()))
        {
            return Err(Error::AcrossMalformed {
                part: "resolve",
                problem: "a committed resolution that does not say where every prepare landed",
            });
        }
        self.writes.clear();
        let Some(coordinator) = self.buffer_resolutions(transaction, committed, records)? else {
            return Ok(None);
        };
        self.across = Some(Work {
            across: Across {
                transaction,
                part: Part::Resolve { committed },
            },
            coordinator,
            seen: None,
            participants: if committed {
                participants.to_vec()
            } else {
                Vec::new()
            },
        });
        self.commit_placed().map(Some)
    }

    /// Buffer the resolution of `transaction`'s intents on `records` — every
    /// intent of it this node holds when none is named — and answer the
    /// coordinator's range the intents name, or `None` when none is left.
    fn buffer_resolutions(
        &mut self,
        transaction: TransactionId,
        committed: bool,
        records: &[RecordAddress],
    ) -> Result<Option<Reach>> {
        // No records named: every intent of the transaction this node holds
        // in a range it leads, read off its index — how a participant resolves
        // after the coordinator that knew the addresses is gone (D7). A copy of
        // another leader's intent follows that leader's resolution: resolved
        // here with the rest, the commit would span two leaders and be refused,
        // pass after pass, leaving this node's own intents standing with it.
        let every_held = records.is_empty();
        let held;
        let records = if every_held {
            held = self.store.intents_of(transaction)?;
            held.as_slice()
        } else {
            records
        };
        // The coordinator's range is read off the intents themselves, which
        // every prepare stamped with it: a resolution cannot name another.
        let mut coordinator = None;
        for address in records {
            let Some((intent, named)) = self.intent_of(address, transaction)? else {
                continue;
            };
            coordinator.get_or_insert(named);
            let value = if committed {
                intent
            } else {
                // Named, never written: an aborted resolution drops the intent.
                RecordValue::Tombstone
            };
            self.writes.insert(address.clone(), value);
        }
        if every_held && !self.writes.is_empty() {
            // Buffered first, so the placement knows every table they fall in.
            let placement = self.placement()?;
            let mut elsewhere = Vec::new();
            for address in self.writes.keys() {
                let home =
                    crate::catalog::home_of(&LogRecord::new(vec![tessari_encoding::Mutation {
                        namespace: address.namespace,
                        database: address.database,
                        table: address.table,
                        id: address.id.clone(),
                        shard: placement.shard_of(address),
                        value: tessari_encoding::StampedValue::new(RecordValue::Tombstone),
                    }]))?;
                if !self.store.leads(home)? {
                    elsewhere.push(address.clone());
                }
            }
            for address in elsewhere {
                self.writes.remove(&address);
            }
            if self.writes.is_empty() {
                return Ok(None);
            }
        }
        Ok(coordinator)
    }

    /// The value `transaction` holds as an intent on `address`, and the
    /// coordinator's range the intent names, if it holds one.
    fn intent_of(
        &self,
        address: &RecordAddress,
        transaction: TransactionId,
    ) -> Result<Option<(RecordValue, Reach)>> {
        let Some((_, provenance, value)) = self.newest_stored_value(address)? else {
            return Ok(None);
        };
        Ok(provenance
            .filter(|provenance| provenance.provisional && provenance.transaction == transaction)
            .map(|provenance| (value, provenance.coordinator)))
    }

    /// Split this transaction's writes into the homes they fall in, name who
    /// leads each, read this node's position of each home's log, and then —
    /// in that order — run the conflict check on this node's copy (ADR-0112
    /// D3a). The participant leaders make the other half.
    ///
    /// # Errors
    ///
    /// [`Error::AcrossNotHeldHere`] for a home this node holds no copy of, a
    /// conflict on this copy, and the substrate's failures.
    pub fn across_plan(&self) -> Result<Vec<AcrossPart>> {
        let me = self.store.node_identity()?.id;
        let placement = self.placement()?;
        let record = self.log_record(me, &placement)?;
        let mut homes: std::collections::BTreeMap<Reach, Vec<tessari_encoding::Mutation>> =
            std::collections::BTreeMap::new();
        for mutation in record.mutations() {
            let home = crate::catalog::home_of(&LogRecord::new(vec![mutation.clone()]))?;
            homes.entry(home).or_default().push(mutation.clone());
        }
        let mut reading = self.store.begin()?;
        let catalog = crate::catalog::Catalog::new(&mut reading);
        let held = catalog.leaderships()?;
        let placed: std::collections::BTreeSet<Reach> = catalog
            .replicas()?
            .into_iter()
            .filter_map(|peer| peer.leads)
            .collect();
        drop(reading);
        let served = self.store.served();
        let mut parts = Vec::with_capacity(homes.len());
        for (home, writes) in homes {
            if served.is_some_and(|over| !over.contains(home)) {
                return Err(Error::AcrossNotHeldHere { range: home });
            }
            let leader = match self.store.led(&held, &placed, home, &me)? {
                crate::store::Led::Elsewhere(leader) => Some(leader.node),
                crate::store::Led::Here | crate::store::Led::Unled | crate::store::Led::Shared => {
                    None
                }
            };
            // The log a commit into this home is filed in — the line's, under a
            // leadership, as `settle` decides it — read on this node's copy.
            let log = if self.store.epoch_under(&placed, home) > tessari_types::Epoch::ZERO
                || leader.is_some()
            {
                LogId::line(home)
            } else {
                self.store.own_log(home)?
            };
            let seen = self.store.committed_tail(log)?;
            parts.push(AcrossPart {
                home,
                leader,
                seen,
                writes,
            });
        }
        // After every position was read: a commit landing between a read and
        // this check is either below its position — and seen here — or above
        // it, and seen by the participant's walk.
        self.check_for_conflicts()?;
        Ok(parts)
    }

    /// Mark a record this commit is about to write with what it is, and every
    /// version in it with where it came from.
    pub(super) fn mark_across(&self, record: LogRecord) -> LogRecord {
        let Some(work) = &self.across else {
            return record;
        };
        let provisional = if work.across.part.prepares() {
            true
        } else if work.across.part.resolution() == Some(true) {
            false
        } else {
            // A decision, a forgetting and a landed part have no versions, and
            // an aborted resolution's are never written.
            return record.across(work.across.clone());
        };
        let provenance = Provenance {
            transaction: work.across.transaction,
            provisional,
            coordinator: work.coordinator,
            participants: work.participants.clone(),
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

    /// The coordinator's range when this commit writes the transaction's
    /// record, which a decision does with no record of its own and which must
    /// still be admitted into that range.
    pub(super) fn decision_range(&self) -> Option<Reach> {
        self.across
            .as_ref()
            .filter(|work| work.across.part.record().is_some())
            .map(|work| work.coordinator)
    }

    /// Whether `provenance` is an intent this commit itself resolves, which
    /// the conflict check must not refuse it for.
    pub(super) fn resolves(&self, provenance: Option<&Provenance>) -> bool {
        match (&self.across, provenance) {
            (Some(work), Some(provenance)) => {
                work.across.part.resolution().is_some()
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
                        with: crate::ConflictWith::Commit,
                    });
                }
            }
        }
        Ok(())
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
