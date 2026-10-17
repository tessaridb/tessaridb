//! Status recovery of a `STAGING` record (ADR-0112 D14c): whether every part
//! of a transaction committed in parallel is held, asked of each participant
//! range's leader.
//!
//! A staging transaction is committed exactly when every participant's prepare
//! is held by a majority of its range (D14a). Recovery therefore asks each part
//! not already known landed, and — when it may stop the transaction — bars a
//! part that has not landed before saying so, so that an implicit commit and
//! an abort can never both happen. It decides nothing itself: the caller writes
//! the outcome by compare-and-set on `STAGING`, which loses to whoever wrote
//! one first.
//!
//! Three callers share it, each with its own way of reaching a range's leader:
//! the coordinator after a refused or unanswered prepare (D14d), housekeeping
//! on a record past its deadline, and a reader that meets the transaction
//! (D14e) — the last never barring, so a read never aborts a live transaction.

use tessari_encoding::{Decision, TransactionId, TransactionRecord};
use tessari_types::Reach;

use super::{AcrossAnswer, AcrossAsk};

/// What recovery found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Every part is held: the record as it is to stand, `COMMITTED`, with
    /// where each part landed.
    Committed(TransactionRecord),
    /// This range's part had not landed and is now barred there for good: the
    /// transaction can only abort.
    Barred(Reach),
    /// This range's part has not landed, and was not barred: not committed
    /// yet, and it may still be.
    Missing(Reach),
    /// A part nobody could answer for: the outcome is not known from here.
    Unknown(String),
}

/// Recover `record`, which stages, asking each participant range's leader
/// through `ask`: every part but those the record already knows landed.
/// `prevent` bars a part that has not landed rather than only reporting it.
pub async fn recover_staging(
    transaction: TransactionId,
    record: &TransactionRecord,
    prevent: bool,
    mut ask: impl AsyncFnMut(Reach, &AcrossAsk) -> Result<AcrossAnswer, String>,
) -> Recovery {
    let mut staged = Staged::new(transaction, record, prevent);
    while let Some((range, asked)) = staged.next_ask() {
        if let Some(recovered) = staged.heard(ask(range, &asked).await) {
            return recovered;
        }
    }
    staged.finish()
}

/// The recovery of one staging record, asked a part at a time.
///
/// [`recover_staging`] with the asking written by the caller:
/// `while let Some((range, asked)) = staged.next_ask()`, each answer handed to
/// [`Staged::heard`], and [`Staged::finish`] once nothing is left to ask.
pub struct Staged {
    transaction: TransactionId,
    prevent: bool,
    committed: TransactionRecord,
    /// The next participant to ask about.
    at: usize,
}

impl Staged {
    /// The recovery of `record`, which stages.
    #[must_use]
    pub fn new(transaction: TransactionId, record: &TransactionRecord, prevent: bool) -> Self {
        Self {
            transaction,
            prevent,
            committed: TransactionRecord {
                decision: Decision::Committed,
                ..record.clone()
            },
            at: 0,
        }
    }

    /// The next range to ask and what to ask it, or `None` once every part
    /// not already known to have landed has been asked.
    pub fn next_ask(&mut self) -> Option<(Reach, AcrossAsk)> {
        while let Some(participant) = self.committed.participants.get(self.at) {
            if participant.prepared_at.is_none() {
                return Some((
                    participant.range,
                    AcrossAsk::Bar {
                        transaction: self.transaction,
                        range: participant.range,
                        prevent: self.prevent,
                    },
                ));
            }
            self.at = self.at.saturating_add(1);
        }
        None
    }

    /// The answer to the last ask: `Some` when it settles the recovery, `None`
    /// when the next part is to be asked.
    pub fn heard(&mut self, answer: Result<AcrossAnswer, String>) -> Option<Recovery> {
        let participant = self.committed.participants.get_mut(self.at)?;
        self.at = self.at.saturating_add(1);
        match answer {
            Ok(AcrossAnswer::Landed(Some(at))) => {
                participant.prepared_at = Some(at);
                None
            }
            Ok(AcrossAnswer::Landed(None)) if self.prevent => {
                Some(Recovery::Barred(participant.range))
            }
            Ok(AcrossAnswer::Landed(None)) => Some(Recovery::Missing(participant.range)),
            Ok(other) => Some(Recovery::Unknown(format!(
                "{:?} answered {other:?} when asked where a part landed",
                participant.range
            ))),
            Err(why) => Some(Recovery::Unknown(why)),
        }
    }

    /// The record with every part landed, once nothing is left to ask.
    #[must_use]
    pub fn finish(self) -> Recovery {
        Recovery::Committed(self.committed)
    }
}

#[cfg(test)]
mod tests;
