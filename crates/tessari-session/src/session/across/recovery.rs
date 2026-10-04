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
/// through `ask` — every part but those the record already knows landed.
/// `prevent` bars a part that has not landed rather than only reporting it.
pub fn recover_staging(
    transaction: TransactionId,
    record: &TransactionRecord,
    prevent: bool,
    mut ask: impl FnMut(Reach, &AcrossAsk) -> Result<AcrossAnswer, String>,
) -> Recovery {
    let mut committed = TransactionRecord {
        decision: Decision::Committed,
        ..record.clone()
    };
    for participant in &mut committed.participants {
        if participant.prepared_at.is_some() {
            continue;
        }
        let asked = AcrossAsk::Bar {
            transaction,
            range: participant.range,
            prevent,
        };
        match ask(participant.range, &asked) {
            Ok(AcrossAnswer::Landed(Some(at))) => participant.prepared_at = Some(at),
            Ok(AcrossAnswer::Landed(None)) if prevent => {
                return Recovery::Barred(participant.range);
            }
            Ok(AcrossAnswer::Landed(None)) => return Recovery::Missing(participant.range),
            Ok(other) => {
                return Recovery::Unknown(format!(
                    "{:?} answered {other:?} when asked where a part landed",
                    participant.range
                ));
            }
            Err(why) => return Recovery::Unknown(why),
        }
    }
    Recovery::Committed(committed)
}

#[cfg(test)]
mod tests;
