//! Finishing transactions across leaders nobody came back to (ADR-0112 D7).
//!
//! A coordinator that dies leaves its record `PENDING` and its intents
//! standing. Nothing waits for it: every node, on its housekeeping cadence,
//! aborts the overdue records of the ranges it leads, and resolves the intents
//! it holds once their record's range's leader says how — never from a copy of
//! the record, which may hold a decision no majority does yet. Each participant
//! finishes its own intents, so nobody needs the addresses only the coordinator
//! knew.

use std::collections::BTreeSet;

use tessari_session::{AcrossAnswer, AcrossAsk, Recovery};
use tessari_storage::Decision;

use crate::{Db, Result};

/// What one housekeeping pass over transactions across leaders did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettledAcross {
    /// Overdue records this node aborted.
    pub aborted: usize,
    /// Overdue `STAGING` records this node found committed — every part
    /// landed — and wrote so (ADR-0112 D14c).
    pub committed: usize,
    /// Transactions whose intents here were resolved.
    pub resolved: usize,
    /// Transactions whose outcome could not be asked for this pass.
    pub unreachable: usize,
    /// Decided records this node forgot, every participant's intents being
    /// gone for good (ADR-0112 D12).
    pub forgotten: usize,
    /// The last refusal met this pass, for the node to log.
    pub last_refusal: Option<String>,
}

impl Db {
    /// Abort the overdue records this node leads, and resolve the intents it
    /// holds whose record has decided.
    ///
    /// # Errors
    ///
    /// The store's failure to list its records or intents; a refusal for one
    /// transaction is logged and passes on to the next.
    pub fn settle_across(&self) -> Result<SettledAcross> {
        self.settle_across_at(now_millis())
    }

    /// [`Self::settle_across`] as of `now`, milliseconds since the Unix epoch.
    pub(crate) fn settle_across_at(&self, now: u64) -> Result<SettledAcross> {
        let store = self.store();
        let mut settled = SettledAcross::default();
        // A leader that did not answer is not asked again this pass: a node
        // that hangs costs one peer link's patience per pass, not one per
        // transaction it coordinates.
        let mut silent: BTreeSet<[u8; tessari_storage::NODE_ID_LEN]> = BTreeSet::new();
        // First, so a record resolved in this pass is forgotten in the next:
        // forgetting asks every participant, and a pass that has just
        // resolved here would only be told so a moment later.
        self.forget_decided(&mut settled, &mut silent)?;
        let pending = store.pending_across()?;
        let pending_seen = pending.len();
        for (transaction, record) in pending {
            let Some(coordinator) = record.participants.first().map(|part| part.range) else {
                continue;
            };
            if record.deadline > now || store.leader_of(coordinator)?.is_some() {
                continue;
            }
            if record.decision == Decision::Staging {
                self.recover(transaction, &record, &mut settled, &mut silent)?;
                continue;
            }
            match self.session().answer_across(&AcrossAsk::Settle {
                transaction,
                coordinator,
            }) {
                Ok(AcrossAnswer::Outcome(decided)) if decided.decision == Decision::Aborted => {
                    settled.aborted = settled.aborted.saturating_add(1);
                }
                Ok(_) => {}
                Err(why) => settled.last_refusal = Some(why.to_string()),
            }
        }
        let standing = store.standing_across()?;
        let standing_seen = standing.len();
        let young = self.young_standing(&standing, now)?;
        for (transaction, coordinator) in standing {
            // Asked of the record's range's leader even when this node holds
            // a copy of the record: a copy, or the leader's own read, may be a
            // decision no majority holds yet, and only the leader's answer
            // writes it again at one first (ADR-0112 D4).
            //
            // An intent found less than a lapse ago may stand before the record
            // that decides it is written — a prepare outran its begin (D13a) —
            // so its leader is asked without aborting: a decided record is
            // answered all the same, and an absent one only once a lapse has
            // passed, as D7 aborts it.
            let asked = if young.contains(&transaction) {
                AcrossAsk::Lookup {
                    transaction,
                    coordinator,
                }
            } else {
                AcrossAsk::Settle {
                    transaction,
                    coordinator,
                }
            };
            let record = match self.ask_leader_of(coordinator, &asked, &mut silent)? {
                // Undecided, `STAGING` included: it may be committed
                // implicitly already, and only recovery decides it (D14c).
                Some(Ok(AcrossAnswer::Outcome(record))) if record.decision.is_decided() => record,
                Some(Ok(_)) => continue,
                Some(Err(why)) => {
                    settled.unreachable = settled.unreachable.saturating_add(1);
                    settled.last_refusal = Some(why);
                    continue;
                }
                None => {
                    settled.unreachable = settled.unreachable.saturating_add(1);
                    continue;
                }
            };
            let committed = record.decision == Decision::Committed;
            // Refused where this node does not lead the intents' home, which is
            // the design: their leader resolves them and this copy follows.
            match self.session().answer_across(&AcrossAsk::Resolve {
                transaction,
                committed,
                records: Vec::new(),
                participants: if committed {
                    record.participants
                } else {
                    Vec::new()
                },
            }) {
                Ok(AcrossAnswer::Resolved(Some(_))) => {
                    settled.resolved = settled.resolved.saturating_add(1);
                }
                Ok(_) => {}
                Err(why) => settled.last_refusal = Some(why.to_string()),
            }
        }
        // What this pass left standing, for the operator (ADR-0112 D11): the
        // pass has just walked both, so publishing it costs no second walk,
        // and a scrape reads a number instead of walking them itself.
        let left = |seen: usize, finished: usize| {
            u64::try_from(seen.saturating_sub(finished)).unwrap_or(u64::MAX)
        };
        store.across_sampled(
            left(pending_seen, settled.aborted),
            left(standing_seen, settled.resolved),
        );
        Ok(settled)
    }
}

impl Db {
    /// Recover an overdue `STAGING` record this node leads (ADR-0112 D14c):
    /// every part landed → `COMMITTED`; one not landed is barred at its range
    /// first → `ABORTED`. The decision by compare-and-set on `STAGING`, so a
    /// coordinator concluding meanwhile wins, and loses nothing.
    fn recover(
        &self,
        transaction: tessari_storage::TransactionId,
        record: &tessari_storage::TransactionRecord,
        settled: &mut SettledAcross,
        silent: &mut BTreeSet<[u8; tessari_storage::NODE_ID_LEN]>,
    ) -> Result<()> {
        let mut failed = Ok(());
        let recovered =
            tessari_session::recover_staging(transaction, record, true, |range, asked| match self
                .ask_leader_of(range, asked, silent)
            {
                Ok(Some(answered)) => answered,
                Ok(None) => Err(format!("the leader of {range:?} did not answer this pass")),
                Err(why) => {
                    let said = why.to_string();
                    failed = Err(why);
                    Err(said)
                }
            });
        failed?;
        let decided = match recovered {
            Recovery::Committed(committed) => committed,
            Recovery::Barred(_) => tessari_storage::TransactionRecord {
                decision: Decision::Aborted,
                ..record.clone()
            },
            Recovery::Missing(_) => return Ok(()),
            Recovery::Unknown(why) => {
                settled.unreachable = settled.unreachable.saturating_add(1);
                settled.last_refusal = Some(why);
                return Ok(());
            }
        };
        let committed = decided.decision == Decision::Committed;
        match self.session().answer_across(&AcrossAsk::Decide {
            transaction,
            record: decided,
        }) {
            Ok(_) if committed => settled.committed = settled.committed.saturating_add(1),
            Ok(_) => settled.aborted = settled.aborted.saturating_add(1),
            Err(why) => settled.last_refusal = Some(why.to_string()),
        }
        Ok(())
    }

    /// Forget each decided record whose range this node leads once every
    /// participant answers that its intents are gone for good (ADR-0112 D12).
    fn forget_decided(
        &self,
        settled: &mut SettledAcross,
        silent: &mut BTreeSet<[u8; tessari_storage::NODE_ID_LEN]>,
    ) -> Result<()> {
        let store = self.store();
        for (transaction, record) in store.decided_across()? {
            let Some(coordinator) = record.participants.first().map(|part| part.range) else {
                continue;
            };
            if store.leader_of(coordinator)?.is_some() {
                continue;
            }
            let mut gone = true;
            for participant in &record.participants {
                let asked = AcrossAsk::Holds {
                    transaction,
                    range: participant.range,
                };
                match self.ask_leader_of(participant.range, &asked, silent)? {
                    Some(Ok(AcrossAnswer::Holding(false))) => {}
                    Some(Err(why)) => {
                        settled.last_refusal = Some(why);
                        gone = false;
                        break;
                    }
                    _ => {
                        gone = false;
                        break;
                    }
                }
            }
            if !gone {
                continue;
            }
            match self.session().answer_across(&AcrossAsk::Forget {
                transaction,
                coordinator,
            }) {
                Ok(AcrossAnswer::Forgotten(_)) => {
                    settled.forgotten = settled.forgotten.saturating_add(1);
                }
                Ok(_) => {}
                Err(why) => settled.last_refusal = Some(why.to_string()),
            }
        }
        Ok(())
    }

    /// The standing transactions this node first found standing less than a
    /// lapse before `now`, recording when it first found each new one and
    /// forgetting those no longer standing.
    fn young_standing(
        &self,
        standing: &[(tessari_storage::TransactionId, tessari_types::Reach)],
        now: u64,
    ) -> Result<BTreeSet<tessari_storage::TransactionId>> {
        let lapse = tessari_session::across_lapse_millis(self.store())?;
        let mut since = self
            .standing_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        since.retain(|transaction, _| standing.iter().any(|(held, _)| held == transaction));
        Ok(standing
            .iter()
            .filter(|(transaction, _)| {
                let first = *since.entry(*transaction).or_insert(now);
                now.saturating_sub(first) <= lapse
            })
            .map(|(transaction, _)| *transaction)
            .collect())
    }

    /// Ask `range`'s leader — this node itself when it leads it. `None` when
    /// that leader did not answer earlier in this pass and is not asked again;
    /// a leader that does not answer now joins `silent`.
    fn ask_leader_of(
        &self,
        range: tessari_types::Reach,
        asked: &AcrossAsk,
        silent: &mut BTreeSet<[u8; tessari_storage::NODE_ID_LEN]>,
    ) -> Result<Option<std::result::Result<AcrossAnswer, String>>> {
        let leader = match self.store().leader_of(range)? {
            Some(node) => Some(node),
            // A range this node holds none of: its leadership rows live in
            // logs this node never collects, so its catalog cannot say who
            // leads it — and this node must not answer for it. The peers'
            // greetings say (ADR-0112 D13d).
            None if self
                .store()
                .served()
                .is_some_and(|over| !over.contains(range)) =>
            {
                match self.heard_leading(range)? {
                    Some(node) => Some(node),
                    None => {
                        return Ok(Some(Err(format!("no leader of {range:?} has been heard"))));
                    }
                }
            }
            None => None,
        };
        let answered = match leader {
            Some(node) if silent.contains(&node) => return Ok(None),
            None => self
                .session()
                .answer_across(asked)
                .map_err(|why| why.to_string()),
            Some(node) => match self.participants.get() {
                Some(carrier) => carrier
                    .ask(node, None, asked)
                    .map_err(|refused| refused.reason),
                None => Err("this node carries nothing to other nodes".to_owned()),
            },
        };
        if answered.is_err() {
            silent.extend(leader);
        }
        Ok(Some(answered))
    }
}

impl Db {
    /// The node heard leading the line `range` is judged on — the most
    /// specific placed range containing it, else the store line.
    fn heard_leading(
        &self,
        range: tessari_types::Reach,
    ) -> Result<Option<[u8; tessari_storage::NODE_ID_LEN]>> {
        let Some(elsewhere) = self.elsewhere.get() else {
            return Ok(None);
        };
        let mut reading = self.store().begin()?;
        let placed: BTreeSet<tessari_types::Reach> = tessari_storage::Catalog::new(&mut reading)
            .replicas()?
            .into_iter()
            .filter_map(|peer| peer.leads)
            .collect();
        reading.rollback();
        let line = tessari_storage::governing(&placed, range);
        let heard = if line == tessari_types::Reach::Store {
            elsewhere.writable()
        } else {
            elsewhere.leading(line)
        };
        Ok(heard.map(|peer| peer.node))
    }
}

/// Answers a reader that meets an intent its copy cannot decide by asking the
/// record's range's leader (ADR-0112 D13d).
///
/// Weak, because the store this is installed on is held by the `Db` it asks
/// through: a strong pointer back would keep both alive after the node let go.
#[derive(Debug)]
struct LeadersDecide(std::sync::Weak<Db>);

impl tessari_storage::Decisions for LeadersDecide {
    fn decided(
        &self,
        transaction: tessari_storage::TransactionId,
        coordinator: tessari_types::Reach,
    ) -> Option<tessari_storage::TransactionRecord> {
        let db = self.0.upgrade()?;
        let asked = AcrossAsk::Lookup {
            transaction,
            coordinator,
        };
        let mut silent = BTreeSet::new();
        match db.ask_leader_of(coordinator, &asked, &mut silent) {
            Ok(Some(Ok(AcrossAnswer::Outcome(record)))) if record.decision.is_decided() => {
                Some(record)
            }
            // Committed implicitly only if every part is held — asked without
            // barring, so a read never aborts a live transaction (D14e).
            Ok(Some(Ok(AcrossAnswer::Outcome(record)))) if record.decision == Decision::Staging => {
                let recovered = tessari_session::recover_staging(
                    transaction,
                    &record,
                    false,
                    |range, asked| match db.ask_leader_of(range, asked, &mut silent) {
                        Ok(Some(answered)) => answered,
                        Ok(None) => Err(format!("the leader of {range:?} did not answer")),
                        Err(why) => Err(why.to_string()),
                    },
                );
                match recovered {
                    Recovery::Committed(committed) => Some(committed),
                    Recovery::Barred(_) | Recovery::Missing(_) | Recovery::Unknown(_) => None,
                }
            }
            _ => None,
        }
    }
}

impl Db {
    /// Let this node's readers ask a transaction's record leader when their
    /// copy of the record cannot decide an intent (ADR-0112 D13d) — installed
    /// with the carriage that reaches the leaders.
    pub fn decide_reads_through_leaders(self: &std::sync::Arc<Self>) {
        self.store()
            .answer_decisions_with(std::sync::Arc::new(LeadersDecide(
                std::sync::Arc::downgrade(self),
            )));
    }
}

/// Milliseconds since the Unix epoch; zero for a clock before it.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
