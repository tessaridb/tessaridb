//! A superseded leader's answer is not applied (ADR-0107, Q-879 H1).
//!
//! Raft's rule that a follower rejects an append from a term older than the one
//! it holds, in this store's terms: an answer states the leadership its server
//! held when it answered (`Collected::epoch`), and this node's own copy of that
//! log holds records written under the leaderships that actually wrote to it. A
//! stated leadership below the one this node's copy already reaches belongs to
//! a node the line has moved past, and the records it serves may be ones its
//! successor never held — applying them is how a follower's copy forks from the
//! line's one history.
//!
//! # A leadership counts once it has written, not once it has been shown
//!
//! The judge is the copy's tail and never this node's voting memory. A voter is
//! shown every epoch a candidate stands at, won or not, and a campaign that
//! lost is no evidence that the leader it challenged was superseded: judged
//! against what was shown, one lost ballot left a follower refusing the line's
//! live leader for good, and the write that leader acknowledged was lost when
//! the line next changed hands (measured 2026-10-02, the paused-leader test,
//! run 42). A record in this node's copy at a newer leadership is the line's
//! own proof that the newer leadership exists.
//!
//! An answer that states no leadership is left alone: its server leads no line
//! governing that home, which is the answer a store-line leader gives for a
//! home inside a range somebody else leads, and nothing about it is stale.

use super::Collected;
use crate::error::{Error, Result};
use tessari_storage::Store;

/// `fetched`, with every answer from a superseded leader replaced by
/// [`Error::Deposed`].
///
/// Each answer is judged against this node's copy of the log it answers for,
/// because one round can carry the logs of several lines and each line counts
/// its own leaderships. A copy whose tail cannot be read refuses the answer
/// rather than applying what could not be judged.
pub(super) fn refused(into: &Store, fetched: Vec<Result<Collected>>) -> Vec<Result<Collected>> {
    fetched
        .into_iter()
        .map(|answer| {
            let collected = answer?;
            let Some(stated) = collected.epoch else {
                return Ok(collected);
            };
            let newest = into
                .tail_leadership(collected.log)
                .map_err(|why| Error::Refused {
                    message: format!(
                        "this node cannot say which leadership its copy reaches: {why}"
                    ),
                    class: None,
                })?;
            if stated < newest {
                return Err(Error::Deposed { stated, newest });
            }
            Ok(collected)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessari_storage::{LEASE_TTL, Lease, LogId};
    use tessari_types::{Epoch, Sequence};
    use tessaridb::Db;

    fn stating(log: LogId, epoch: Option<u64>) -> Result<Collected> {
        Ok(Collected {
            log,
            previous: Epoch::ZERO,
            records: Vec::new(),
            stopped_early: false,
            over: None,
            order: Some(Sequence::new(1)),
            epoch: epoch.map(Epoch::new),
        })
    }

    /// The one log `db` has written, after one statement.
    fn written(db: &Db, statement: &str) -> LogId {
        db.session().run(statement).expect("the write commits");
        *db.store()
            .logs()
            .expect("the logs")
            .first()
            .expect("the write filed a log")
    }

    #[test]
    fn an_answer_below_the_leadership_this_copy_reaches_is_refused_and_a_current_one_applies() {
        // Raft's `term < currentTerm`: this node's copy holds a record written
        // under leadership 5, so a node still answering under 3 has been
        // superseded — what it serves may be records leadership 5 never held. An
        // answer stating 5 or more is the line's current leader, and one stating
        // nothing leads no line governing that home and is not judged here.
        let db = Db::in_memory().expect("an in-memory store");
        db.store().hold(Epoch::new(5), Lease::taken(LEASE_TTL));
        let line = written(&db, "DEFINE NAMESPACE led");
        assert_eq!(
            db.store().tail_leadership(line).expect("its tail"),
            Epoch::new(5)
        );

        let judged = refused(
            db.store(),
            vec![
                stating(line, Some(3)),
                stating(line, Some(5)),
                stating(line, Some(6)),
                stating(line, None),
            ],
        );
        assert!(
            matches!(
                judged[0],
                Err(Error::Deposed { stated, newest })
                    if stated == Epoch::new(3) && newest == Epoch::new(5)
            ),
            "{:?}",
            judged[0]
        );
        assert!(judged[1..].iter().all(Result::is_ok), "{judged:?}");
    }

    #[test]
    fn a_copy_no_newer_leadership_has_written_applies_the_leader_that_answers() {
        // Run 42 at the level it was born: this node's copy reaches no record
        // written under a leadership, so an answer stating 6 is applied —
        // whatever ballots this node was shown at 7, nothing at 7 has written,
        // and nothing says leadership 6 is over.
        let db = Db::in_memory().expect("an in-memory store");
        let own = written(&db, "DEFINE NAMESPACE own");
        let judged = refused(db.store(), vec![stating(own, Some(6))]);
        assert!(judged[0].is_ok(), "{judged:?}");
    }
}
