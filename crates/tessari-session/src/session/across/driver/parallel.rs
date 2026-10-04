//! The coordinator's half of a parallel commit (ADR-0112 D14): answered after
//! one round, finished behind the answer.
//!
//! The begin wrote the record `STAGING`, naming every participant, in the same
//! round as the other prepares. Once every one of them answered *prepared* the
//! transaction is committed implicitly and the caller is told so at once
//! (D14b); the explicit `COMMITTED` record and the resolutions are written
//! behind the answer, by a thread that waits for nobody to hold them — the
//! outcome is already final, and recovery re-derives it from the parts if they
//! are lost (D14c).
//!
//! Anything else — a refusal, or an answer that never came, which the
//! coordinator cannot tell apart — never aborts the record outright: a
//! prepare may have landed after all. The coordinator recovers the record as
//! anyone would, barring every part it did not hear land, and reports what that
//! finds (D14d).

use tessari_encoding::{Decision, NODE_ID_LEN, TransactionId, TransactionRecord};
use tessari_ql::Span;
use tessari_storage::{AcrossPart, Store};

use super::super::{
    AcrossAnswer, AcrossAsk, AcrossRefusal, Participants, Recovery, recover_staging,
};
use super::records_of;
use crate::error::{Error, Result};
use crate::session::Session;

/// One record still to write behind the answer, and the leader to write it.
type Behind = (Option<[u8; NODE_ID_LEN]>, AcrossAsk);

impl Session<'_> {
    /// Finish a parallel commit whose prepare round has answered: `record`
    /// stages and names where each part it heard land landed; `refused` is the
    /// first part that did not answer *prepared*.
    pub(super) fn finish_parallel(
        &mut self,
        store: &Store,
        carrying: &std::sync::Arc<dyn Participants>,
        parts: &[AcrossPart],
        (id, record, refused): (TransactionId, TransactionRecord, Option<AcrossRefusal>),
        user: Option<&tessari_storage::UserDefinition>,
        span: Span,
    ) -> Result<()> {
        let Some(refusal) = refused else {
            let committed = TransactionRecord {
                decision: Decision::Committed,
                ..record
            };
            behind(store, carrying, user, concluding(parts, id, &committed));
            return Ok(());
        };
        let carrier = carrying.as_ref();
        let Some(first) = parts.first() else {
            return Ok(());
        };
        if record
            .participants
            .first()
            .is_none_or(|own| own.prepared_at.is_none())
        {
            // The begin itself was not heard: the record may be absent, staging
            // or decided. Before its deadline nothing but this coordinator
            // decides it, so an absent one is aborted where it stands — which
            // also refuses a begin still on its way. Past it, an absent record
            // may be one a recovery committed and forgot (D12): not knowing
            // which, nothing is written and the caller hears so.
            let asked = if super::super::now_millis() < record.deadline {
                AcrossAsk::Settle {
                    transaction: id,
                    coordinator: first.home,
                }
            } else {
                AcrossAsk::Lookup {
                    transaction: id,
                    coordinator: first.home,
                }
            };
            match self.ask_one(carrier, first.leader, user, &asked) {
                Ok(AcrossAnswer::Outcome(standing)) => match standing.decision {
                    Decision::Staging => {}
                    Decision::Aborted => {
                        behind(store, carrying, user, aborting(parts, id));
                        return Err(Error::AcrossAborted { refusal, span });
                    }
                    // Recovered and committed by someone else, who resolves.
                    Decision::Committed => return Ok(()),
                    Decision::Pending => {
                        return Err(Error::AcrossInDoubt {
                            reason: format!(
                                "the record could not be found past its deadline after {refusal}"
                            ),
                            span,
                        });
                    }
                },
                Ok(other) => {
                    return Err(Error::AcrossInDoubt {
                        reason: format!("the record's leader answered {other:?}"),
                        span,
                    });
                }
                Err(why) => {
                    return Err(Error::AcrossInDoubt {
                        reason: why.to_string(),
                        span,
                    });
                }
            }
        }
        let recovered = recover_staging(id, &record, true, |range, asked| {
            let leader = parts
                .iter()
                .find(|part| part.home == range)
                .map(|part| part.leader)
                .ok_or_else(|| format!("{range:?} is not a part of this transaction"))?;
            self.ask_one(carrier, leader, user, asked)
                .map_err(|why| why.to_string())
        });
        match recovered {
            Recovery::Committed(committed) => {
                behind(store, carrying, user, concluding(parts, id, &committed));
                Ok(())
            }
            Recovery::Barred(_) => {
                let aborted = AcrossAsk::Decide {
                    transaction: id,
                    record: TransactionRecord {
                        decision: Decision::Aborted,
                        ..record
                    },
                };
                // A part is barred, so the transaction can never commit: the
                // abort is the outcome whether or not this decision is
                // confirmed, and its intents can go.
                if let Err(why) = self.ask_one(carrier, first.leader, user, &aborted) {
                    tracing::info!(
                        error = %why,
                        "a barred cross-leader transaction's abort was not confirmed"
                    );
                }
                behind(store, carrying, user, aborting(parts, id));
                Err(Error::AcrossAborted { refusal, span })
            }
            Recovery::Missing(range) => Err(Error::AcrossInDoubt {
                reason: format!("{range:?} was neither held nor barred"),
                span,
            }),
            Recovery::Unknown(reason) => Err(Error::AcrossInDoubt { reason, span }),
        }
    }
}

/// The records a committed transaction still writes: the coordinator range's
/// conclusion and every other part's resolution.
fn concluding(
    parts: &[AcrossPart],
    id: TransactionId,
    committed: &TransactionRecord,
) -> Vec<Behind> {
    parts
        .iter()
        .enumerate()
        .map(|(at, part)| {
            let asked = if at == 0 {
                AcrossAsk::Conclude {
                    transaction: id,
                    record: committed.clone(),
                    records: records_of(part),
                }
            } else {
                AcrossAsk::Resolve {
                    transaction: id,
                    committed: true,
                    records: records_of(part),
                    participants: committed.participants.clone(),
                }
            };
            (part.leader, asked)
        })
        .collect()
}

/// Every part's intents dropped, the record having aborted.
fn aborting(parts: &[AcrossPart], id: TransactionId) -> Vec<Behind> {
    parts
        .iter()
        .map(|part| {
            let asked = AcrossAsk::Resolve {
                transaction: id,
                committed: false,
                records: records_of(part),
                participants: Vec::new(),
            };
            (part.leader, asked)
        })
        .collect()
}

/// Write `records` on a thread of their own, the caller not waiting: one this
/// node leads straight into the store, the others asked of their leaders. A
/// record that does not land is the transaction's record to finish — status
/// recovery re-derives the outcome from the parts (D14c).
fn behind(
    store: &Store,
    carrying: &std::sync::Arc<dyn Participants>,
    user: Option<&tessari_storage::UserDefinition>,
    records: Vec<Behind>,
) {
    let (store, carrier, user) = (
        store.clone(),
        std::sync::Arc::clone(carrying),
        user.cloned(),
    );
    let spawned = std::thread::Builder::new()
        .name("across-behind".to_owned())
        .spawn(move || {
            std::thread::scope(|scope| {
                for (leader, asked) in &records {
                    let (store, carrier, user) = (&store, &carrier, user.as_ref());
                    scope.spawn(move || {
                        let landed = match leader {
                            None => here(store, asked),
                            Some(node) => carrier
                                .ask(*node, user, asked)
                                .map(drop)
                                .map_err(|why| why.reason),
                        };
                        if let Err(why) = landed {
                            tracing::warn!(
                                error = %why,
                                "a cross-leader record behind the answer did not land and is left to its record"
                            );
                        }
                    });
                }
            });
        });
    if let Err(why) = spawned {
        tracing::warn!(error = %why, "cross-leader records behind the answer left to their record");
    }
}

/// Write one record behind the answer on this node, which leads its range.
fn here(store: &Store, asked: &AcrossAsk) -> std::result::Result<(), String> {
    let written = match asked {
        AcrossAsk::Conclude {
            transaction,
            record,
            records,
        } => store
            .begin()
            .and_then(|writing| writing.conclude_across(*transaction, record.clone(), records))
            .map(drop),
        AcrossAsk::Resolve {
            transaction,
            committed,
            records,
            participants,
        } => store
            .begin()
            .and_then(|writing| {
                writing.resolve_across(*transaction, *committed, records, participants)
            })
            .map(drop),
        other => return Err(format!("{other:?} is not written behind an answer")),
    };
    written.map_err(|why| why.to_string())
}
