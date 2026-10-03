//! Finishing transactions across leaders nobody came back to (ADR-0112 D7).
//!
//! A coordinator that dies leaves its record `PENDING` and its intents
//! standing. Nothing waits for it: every node, on its housekeeping cadence,
//! aborts the overdue records of the ranges it leads, and resolves the intents
//! it holds once their record says how — asking the record's range's leader
//! when it does not hold the record's range itself. Each participant finishes
//! its own intents, so nobody needs the addresses only the coordinator knew.

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
        let now = now_millis();
        for (transaction, record) in store.pending_across()? {
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
                Ok(AcrossAnswer::Outcome(Decision::Aborted)) => {
                    settled.aborted = settled.aborted.saturating_add(1);
                }
                Ok(_) => {}
                Err(why) => settled.last_refusal = Some(why.to_string()),
            }
        }
        for (transaction, coordinator) in store.standing_across()? {
            let committed = match store
                .transaction_record(transaction)?
                .map(|held| held.decision)
            {
                Some(Decision::Committed) => true,
                Some(Decision::Aborted) => false,
                Some(Decision::Pending) | None => {
                    let asked = AcrossAsk::Settle {
                        transaction,
                        coordinator,
                    };
                    let answered = match store.leader_of(coordinator)? {
                        None => self
                            .session()
                            .answer_across(&asked)
                            .map_err(|why| why.to_string()),
                        Some(node) => match self.participants.get() {
                            Some(carrier) => carrier.ask(node, None, &asked),
                            None => Err("this node carries nothing to other nodes".to_owned()),
                        },
                    };
                    match answered {
                        Ok(AcrossAnswer::Outcome(Decision::Committed)) => true,
                        Ok(AcrossAnswer::Outcome(Decision::Aborted)) => false,
                        Ok(_) => continue,
                        Err(why) => {
                            settled.unreachable = settled.unreachable.saturating_add(1);
                            settled.last_refusal = Some(why);
                            continue;
                        }
                    }
                }
            };
            // Refused where this node does not lead the intents' home, which is
            // the design: their leader resolves them and this copy follows.
            match self.session().answer_across(&AcrossAsk::Resolve {
                transaction,
                committed,
                records: Vec::new(),
            }) {
                Ok(AcrossAnswer::Resolved(Some(_))) => {
                    settled.resolved = settled.resolved.saturating_add(1);
                }
                Ok(_) => {}
                Err(why) => settled.last_refusal = Some(why.to_string()),
            }
        }
        Ok(settled)
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
