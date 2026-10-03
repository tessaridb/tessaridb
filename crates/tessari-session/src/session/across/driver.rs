//! The coordinator's half of a transaction across leaders (ADR-0112): run on
//! the node the transaction ran on, which holds its snapshot, its writes and
//! the copies the conflict check is made against.
//!
//! # The order, and why each step waits for the one before
//!
//! The record is written `PENDING` first, so a prepare never lands for a
//! transaction nobody can decide (D7: a record that does not exist after the
//! deadline is an abort). Every prepare is answered only once a majority holds
//! it; the decision is `COMMITTED` only once every prepare answered, and the
//! caller is told only once a majority holds the decision. Resolutions follow
//! and do not hold the caller: a lost one is the record's to finish.
//!
//! # Parallel where it is only waiting
//!
//! Prepares to different leaders and the resolutions afterwards are network
//! round trips that wait on nothing but their own answers, so they run on
//! scoped threads, one per remote leader. A part this node leads itself is
//! written on the calling thread, which is the only one holding the session.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use tessari_constants::ACROSS_LAPSE_ROUNDS;
use tessari_encoding::{
    Decision, NODE_ID_LEN, Participant, TRANSACTION_ID_LEN, TransactionId, TransactionRecord,
};
use tessari_ql::Span;
use tessari_storage::{
    AcrossOutcome, AcrossPart, Catalog, Failover, RecordAddress, Store, Transaction,
};

use super::{AcrossAnswer, AcrossAsk, AcrossRefusal, PartRefused, Participants};
use crate::error::{Error, Result};
use crate::session::Session;

/// One ask's outcome, with the part it was for.
type Answered = std::result::Result<AcrossAnswer, AcrossRefusal>;

impl Session<'_> {
    /// Whether `transaction` must commit across leaders: it writes homes led
    /// by more than one node, or by another node and this one. `None` when an
    /// ordinary commit answers it — one leader, here or elsewhere.
    ///
    /// # Errors
    ///
    /// What [`Transaction::across_plan`] refuses.
    pub(in crate::session) fn across_parts(
        transaction: &Transaction<'_>,
    ) -> Result<Option<Vec<AcrossPart>>> {
        let parts = transaction.across_plan()?;
        let mut leaders: Vec<Option<[u8; NODE_ID_LEN]>> =
            parts.iter().map(|part| part.leader).collect();
        leaders.sort_unstable();
        leaders.dedup();
        Ok((leaders.len() > 1).then_some(parts))
    }

    /// Commit `parts` across their leaders, answering once the outcome is
    /// known and held.
    ///
    /// # Errors
    ///
    /// [`Error::AcrossAborted`] when nothing was committed,
    /// [`Error::AcrossInDoubt`] when the decision was sent and not confirmed,
    /// and [`Error::AcrossUnavailable`] on a node with no carrier.
    pub(in crate::session) fn drive_across(
        &mut self,
        store: &Store,
        transaction: Transaction<'_>,
        parts: Vec<AcrossPart>,
        span: Span,
    ) -> Result<()> {
        let carrier = self
            .participants
            .clone()
            .ok_or(Error::AcrossUnavailable { span })?;
        let lapse = lapse_millis(store)?;
        // The writes travel in `parts`; the snapshot is released now rather
        // than held across every round trip.
        transaction.rollback();
        let user = self.identity.user().cloned();
        let id = fresh_id();
        let answered = self.drive(carrier.as_ref(), &parts, (id, lapse), user.as_ref(), span);
        // Counted where the client hears it (ADR-0112 D11), whichever way.
        store.across_finished(match &answered {
            Ok(()) => AcrossOutcome::Committed,
            Err(Error::AcrossInDoubt { .. }) => AcrossOutcome::InDoubt,
            Err(_) => AcrossOutcome::Aborted,
        });
        answered
    }

    /// The protocol itself, from the `PENDING` record to the resolutions.
    fn drive(
        &mut self,
        carrier: &dyn Participants,
        parts: &[AcrossPart],
        (id, lapse): (TransactionId, u64),
        user: Option<&tessari_storage::UserDefinition>,
        span: Span,
    ) -> Result<()> {
        let Some(first) = parts.first() else {
            return Ok(());
        };
        let (coordinator, coordinator_leader) = (first.home, first.leader);
        let mut record = TransactionRecord {
            decision: Decision::Pending,
            deadline: super::now_millis().saturating_add(lapse),
            participants: parts
                .iter()
                .map(|part| Participant {
                    range: part.home,
                    prepared_at: None,
                })
                .collect(),
        };
        let deciding = |this: &mut Self, record: &TransactionRecord| {
            this.ask_one(
                carrier,
                coordinator_leader,
                user,
                &AcrossAsk::Decide {
                    transaction: id,
                    record: record.clone(),
                },
            )
        };
        if let Err(refusal) = deciding(self, &record) {
            return Err(Error::AcrossAborted { refusal, span });
        }
        let prepares: Vec<AcrossAsk> = parts
            .iter()
            .map(|part| AcrossAsk::Prepare {
                transaction: id,
                coordinator,
                seen: part.seen,
                writes: part.writes.clone(),
            })
            .collect();
        let prepared = self.ask_parts(carrier, parts, user, &prepares);
        let mut refused = None;
        for ((answer, participant), part) in prepared
            .into_iter()
            .zip(&mut record.participants)
            .zip(parts)
        {
            let refusal = match answer {
                Ok(AcrossAnswer::Prepared(at)) => {
                    participant.prepared_at = Some(at);
                    continue;
                }
                // A leader answering out of turn is a mismatch between two
                // builds, not a fault of the caller's writes.
                Ok(other) => AcrossRefusal::There(PartRefused::retriable(format!(
                    "{:?} answered {other:?}",
                    part.home
                ))),
                Err(refusal) => refusal,
            };
            // Said here because the caller may only ever hear that the
            // decision is in doubt, and this is why it was an abort.
            log::info!(
                "a cross-leader prepare for {:?} was refused: {refusal}",
                part.home
            );
            refused.get_or_insert(refusal);
        }
        record.decision = if refused.is_some() {
            Decision::Aborted
        } else {
            Decision::Committed
        };
        let decided = deciding(self, &record);
        let committed = record.decision == Decision::Committed && decided.is_ok();
        // A committed resolution's versions carry where every prepare landed
        // (D6a); an aborted one writes no versions and needs none.
        let participants = if committed {
            record.participants.clone()
        } else {
            Vec::new()
        };
        let resolves: Vec<AcrossAsk> = parts
            .iter()
            .map(|part| AcrossAsk::Resolve {
                transaction: id,
                committed,
                participants: participants.clone(),
                records: part
                    .writes
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
            })
            .collect();
        match (decided, refused) {
            // Aborted, and said so: the intents can go.
            (Ok(_), Some(refusal)) => {
                self.resolve_parts(carrier, parts, user, &resolves);
                Err(Error::AcrossAborted { refusal, span })
            }
            (Ok(_), None) => {
                self.resolve_parts(carrier, parts, user, &resolves);
                Ok(())
            }
            // The record had already been decided — a lapse aborted it while
            // the prepares ran — so the caller hears the abort.
            (Err(refusal), _) if refusal.to_string().contains("already aborted") => {
                Err(Error::AcrossAborted { refusal, span })
            }
            // Sent and not confirmed: nothing is resolved from here, because
            // which way to resolve is exactly what is not known.
            (Err(refusal), _) => Err(Error::AcrossInDoubt {
                reason: refusal.to_string(),
                span,
            }),
        }
    }

    /// Ask the leader of one part — this node itself when it leads it.
    fn ask_one(
        &mut self,
        carrier: &dyn Participants,
        leader: Option<[u8; NODE_ID_LEN]>,
        user: Option<&tessari_storage::UserDefinition>,
        asked: &AcrossAsk,
    ) -> Answered {
        match leader {
            None => self
                .answer_across(asked)
                .map_err(|refused| AcrossRefusal::Here(Box::new(refused))),
            Some(node) => carrier.ask(node, user, asked).map_err(AcrossRefusal::There),
        }
    }

    /// Ask every part's leader, remote ones at once, answers in part order.
    fn ask_parts(
        &mut self,
        carrier: &dyn Participants,
        parts: &[AcrossPart],
        user: Option<&tessari_storage::UserDefinition>,
        asks: &[AcrossAsk],
    ) -> Vec<Answered> {
        let mut answers: Vec<Option<Answered>> = parts.iter().map(|_| None).collect();
        std::thread::scope(|scope| {
            let remote: Vec<_> = parts
                .iter()
                .zip(asks)
                .enumerate()
                .filter_map(|(at, (part, asked))| {
                    part.leader.map(|node| {
                        (
                            at,
                            scope.spawn(move || {
                                carrier.ask(node, user, asked).map_err(AcrossRefusal::There)
                            }),
                        )
                    })
                })
                .collect();
            for (at, (part, asked)) in parts.iter().zip(asks).enumerate() {
                if part.leader.is_none() {
                    answers[at] = Some(
                        self.answer_across(asked)
                            .map_err(|refused| AcrossRefusal::Here(Box::new(refused))),
                    );
                }
            }
            for (at, waiting) in remote {
                answers[at] = Some(waiting.join().unwrap_or_else(|_| {
                    Err(AcrossRefusal::There(PartRefused::retriable(
                        "the ask's thread panicked",
                    )))
                }));
            }
        });
        answers
            .into_iter()
            .map(|answer| {
                answer.unwrap_or_else(|| {
                    Err(AcrossRefusal::There(PartRefused::retriable(
                        "no answer was asked for",
                    )))
                })
            })
            .collect()
    }

    /// Resolve every part, best effort: a resolution that does not land is
    /// the record's to finish (ADR-0112 D7), so a failure here is logged and
    /// changes nothing the caller is told.
    fn resolve_parts(
        &mut self,
        carrier: &dyn Participants,
        parts: &[AcrossPart],
        user: Option<&tessari_storage::UserDefinition>,
        resolves: &[AcrossAsk],
    ) {
        for (answer, part) in self
            .ask_parts(carrier, parts, user, resolves)
            .into_iter()
            .zip(parts)
        {
            if let Err(why) = answer {
                log::warn!(
                    "a cross-leader resolution for {:?} did not land and is left to its record: {why}",
                    part.home
                );
            }
        }
    }
}

/// How long a `PENDING` record stays live, in milliseconds: the failover
/// policy's round, [`ACROSS_LAPSE_ROUNDS`] times.
fn lapse_millis(store: &Store) -> Result<u64> {
    let mut reading = store.begin()?;
    let round = Catalog::new(&mut reading)
        .failover()?
        .map_or(Failover::DEFAULT, |definition| definition.policy)
        .round();
    reading.rollback();
    let millis = u64::try_from(round.as_millis()).unwrap_or(u64::MAX);
    Ok(millis.saturating_mul(u64::from(ACROSS_LAPSE_ROUNDS)))
}

/// Sixteen bytes from the operating system's generator.
fn fresh_id() -> TransactionId {
    let mut bytes = [0_u8; TRANSACTION_ID_LEN];
    OsRng.fill_bytes(&mut bytes);
    TransactionId::new(bytes)
}
