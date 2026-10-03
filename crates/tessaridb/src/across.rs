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

use tessari_session::{AcrossAnswer, AcrossAsk};
use tessari_storage::Decision;

use crate::{Db, Result};

/// What one housekeeping pass over transactions across leaders did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettledAcross {
    /// Overdue records this node aborted.
    pub aborted: usize,
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
        let store = self.store();
        let mut settled = SettledAcross::default();
        // A leader that did not answer is not asked again this pass: a node
        // that hangs costs one peer link's patience per pass, not one per
        // transaction it coordinates.
        let mut silent: BTreeSet<[u8; tessari_storage::NODE_ID_LEN]> = BTreeSet::new();
        let now = now_millis();
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
        for (transaction, coordinator) in standing {
            // Asked of the record's range's leader even when this node holds
            // a copy of the record: a copy, or the leader's own read, may be a
            // decision no majority holds yet, and only the leader's answer
            // writes it again at one first (ADR-0112 D4).
            let asked = AcrossAsk::Settle {
                transaction,
                coordinator,
            };
            let record = match self.ask_leader_of(coordinator, &asked, &mut silent)? {
                Some(Ok(AcrossAnswer::Outcome(record))) if record.decision != Decision::Pending => {
                    record
                }
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

    /// Ask `range`'s leader — this node itself when it leads it. `None` when
    /// that leader did not answer earlier in this pass and is not asked again;
    /// a leader that does not answer now joins `silent`.
    fn ask_leader_of(
        &self,
        range: tessari_types::Reach,
        asked: &AcrossAsk,
        silent: &mut BTreeSet<[u8; tessari_storage::NODE_ID_LEN]>,
    ) -> Result<Option<std::result::Result<AcrossAnswer, String>>> {
        let leader = self.store().leader_of(range)?;
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
