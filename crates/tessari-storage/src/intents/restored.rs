//! A standalone restore settles every transaction across leaders whose outcome
//! its cut fixes (ADR-0112 D9a, D14f, Q-922b).
//!
//! A restored state stands where the snapshot was taken, intents and records
//! included, so the log after it applies on top. That promise decides what may
//! be settled here: only an outcome no later record of the source can contradict.
//! Committed is final — the record says so, a staging record holds every part
//! (an implicit commit, D14a), or a resolved version of the transaction exists,
//! which only a decision writes — and so is aborted. A transaction the cut
//! leaves undecided may still commit on the source, its missing prepare landing
//! after the cut; settling it here would make that log refuse. It stays, as
//! invisible to a reader as it was at the cut, for the log above or for this
//! node's own recovery, which reaches the verdict a reader at the cut reaches.
//!
//! Each settlement is written the way a restored chunk is — through the settle
//! a log apply runs, in no log — so every log still stands at the source's
//! position, and the source's own later records for the transaction pass over
//! what is already settled.

use tessari_encoding::{
    Across, Decision, IntentOfKey, LogRecord, Mutation, Part, Participant, Provenance, RecordKey,
    ResolvedOfKey, StampedValue, StoreKey, StoreValue, TransactionId, TransactionRecord,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::Sequence;

use crate::error::Result;
use crate::store::Store;
use crate::transaction::RecordAddress;

/// What the cut fixed for one transaction.
enum Outcome {
    /// Committed, with every participant and where its part landed, and the
    /// record to write first when the one standing here has not said so.
    Committed {
        participants: Vec<Participant>,
        record: Option<TransactionRecord>,
    },
    Aborted,
}

impl Store {
    /// Settle every transaction across leaders whose outcome the restored cut
    /// fixes, and answer how many were settled. A transaction the cut leaves
    /// undecided is left standing.
    ///
    /// For the end of a standalone restore only: a follower copying its
    /// leader's state follows that leader's log, which settles the rest.
    ///
    /// # Errors
    ///
    /// The store's refusal of a settlement, and whatever the backend or the
    /// codec returns.
    pub fn settle_restored(&self) -> Result<usize> {
        let mut settled = 0_usize;
        for (transaction, coordinator) in self.standing_across()? {
            let Some(outcome) = self.fixed_by_the_cut(transaction)? else {
                continue;
            };
            let participants = match outcome {
                Outcome::Committed {
                    participants,
                    record,
                } => {
                    if let Some(record) = record {
                        self.restore_state_chunk(&LogRecord::new(Vec::new()).across(Across {
                            transaction,
                            part: Part::Decide(record),
                        }))?;
                    }
                    Some(participants)
                }
                Outcome::Aborted => None,
            };
            for address in self.intents_of(transaction)? {
                let Some(intent) = self.intent_held(transaction, &address)? else {
                    continue;
                };
                let value = match &participants {
                    Some(participants) => intent.from_transaction(Provenance {
                        transaction,
                        provisional: false,
                        coordinator,
                        participants: participants.clone(),
                    }),
                    // Named and never written: an aborted resolution drops it.
                    None => intent,
                };
                let resolution = LogRecord::new(vec![Mutation {
                    shard: self.shard_of(&address)?,
                    namespace: address.namespace,
                    database: address.database,
                    table: address.table,
                    id: address.id,
                    value,
                }])
                .across(Across {
                    transaction,
                    part: Part::Resolve {
                        committed: participants.is_some(),
                    },
                });
                self.restore_state_chunk(&resolution)?;
            }
            settled = settled.saturating_add(1);
        }
        Ok(settled)
    }

    /// The outcome the cut fixes for `transaction`, or `None` when it leaves
    /// it undecided.
    fn fixed_by_the_cut(&self, transaction: TransactionId) -> Result<Option<Outcome>> {
        let record = self.transaction_record(transaction)?;
        match record {
            Some(record) if record.decision == Decision::Committed => {
                return Ok(Some(Outcome::Committed {
                    participants: record.participants,
                    record: None,
                }));
            }
            Some(record) if record.decision == Decision::Aborted => {
                return Ok(Some(Outcome::Aborted));
            }
            Some(ref staging) if staging.decision == Decision::Staging => {
                if let Some(participants) = self.every_part_landed(transaction, staging)? {
                    return Ok(Some(Outcome::Committed {
                        record: Some(TransactionRecord {
                            decision: Decision::Committed,
                            deadline: staging.deadline,
                            participants: participants.clone(),
                        }),
                        participants,
                    }));
                }
            }
            _ => {}
        }
        // A resolved version is written only once the record has committed,
        // and names every participant itself (D6a).
        let Some(participants) = self.resolved_participants(transaction)? else {
            return Ok(None);
        };
        Ok(Some(Outcome::Committed {
            record: record.map(|undecided| TransactionRecord {
                decision: Decision::Committed,
                deadline: undecided.deadline,
                participants: participants.clone(),
            }),
            participants,
        }))
    }

    /// `staging`'s participants with where each part landed, when every one
    /// landed here.
    fn every_part_landed(
        &self,
        transaction: TransactionId,
        staging: &TransactionRecord,
    ) -> Result<Option<Vec<Participant>>> {
        let mut landed = Vec::with_capacity(staging.participants.len());
        for participant in &staging.participants {
            let Some(at) = self.part_landed(transaction, participant.range)? else {
                return Ok(None);
            };
            landed.push(Participant {
                range: participant.range,
                prepared_at: participant.prepared_at.or(Some(at)),
            });
        }
        Ok(Some(landed))
    }

    /// The participants a resolved version of `transaction` held here names.
    fn resolved_participants(
        &self,
        transaction: TransactionId,
    ) -> Result<Option<Vec<Participant>>> {
        let found = self.backend().scan(&ScanRequest {
            keyspace: ResolvedOfKey::keyspace(),
            range: KeyRange::prefix(&ResolvedOfKey::prefix_of(transaction)),
            direction: ScanDirection::Forward,
            limit: None,
        })?;
        for (key, version) in found {
            let held = ResolvedOfKey::decode(key.as_slice())?;
            let stored = RecordKey::new(
                held.namespace,
                held.database,
                held.table,
                held.id,
                Sequence::decode(version.as_slice())?,
            );
            let Some(value) = self
                .backend()
                .get(RecordKey::keyspace(), &stored.encode())?
            else {
                continue;
            };
            if let Some(provenance) = StampedValue::decode(value.as_slice())?.provenance()
                && !provenance.participants.is_empty()
            {
                return Ok(Some(provenance.participants.clone()));
            }
        }
        Ok(None)
    }

    /// The intent `transaction` holds on `address`, by its index.
    fn intent_held(
        &self,
        transaction: TransactionId,
        address: &RecordAddress,
    ) -> Result<Option<StampedValue>> {
        let indexed = IntentOfKey {
            transaction,
            namespace: address.namespace,
            database: address.database,
            table: address.table,
            id: address.id.clone(),
        };
        let Some(version) = self
            .backend()
            .get(IntentOfKey::keyspace(), &indexed.encode())?
        else {
            return Ok(None);
        };
        let stored = RecordKey::new(
            address.namespace,
            address.database,
            address.table,
            address.id.clone(),
            Sequence::decode(version.as_slice())?,
        );
        self.backend()
            .get(RecordKey::keyspace(), &stored.encode())?
            .map(|value| StampedValue::decode(value.as_slice()))
            .transpose()
            .map_err(Into::into)
    }

    /// The shard of a split table `address` falls in, as the restored catalog
    /// says.
    fn shard_of(&self, address: &RecordAddress) -> Result<Option<tessari_types::ShardId>> {
        let mut reading = self.begin()?;
        Ok(crate::catalog::Catalog::new(&mut reading)
            .table(address.table)?
            .and_then(|definition| definition.shards)
            .map(|map| map.shard_of(&address.id)))
    }
}
