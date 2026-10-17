//! What each follower holds of a leader's logs, and a commit waiting for copies.
//!
//! # Where the acknowledgement comes from
//!
//! A follower asks for the first position it does NOT hold in each log, on a
//! held stream and in a collection round alike, and it asks only after the
//! last answer was applied and made durable. So `from - 1` in an ask is what
//! that follower holds — the acknowledgement a write waiting for a majority
//! needs already crosses the wire (ADR-0106 D5, D6). This module keeps it.
//!
//! # Only what this leader sent counts
//!
//! A follower's ask is a claim about its own copy, and a copy can hold a
//! position this leader never wrote: a node of equal length whose last record a
//! deposed leadership wrote asks from the same place a faithful one does. Raft
//! moves `matchIndex` only on a reply that passed the consistency check, for
//! this reason. Here the check is the follower's own: it applies a record only
//! after the predecessor the leader stated matched its copy, and it asks past a
//! record only once it applied it. So an ask counts up to the highest position
//! this leader SENT that follower and no further — what it holds from anywhere
//! else, it holds on its own word.
//!
//! # The latest report, not the highest
//!
//! For the reason [`crate::followers`] gives: a follower that asks from an
//! EARLIER position holds less than it did — a follower that re-seeded after a
//! fork. Counting its old, higher mark toward a majority would acknowledge a
//! write on the strength of a copy that no longer exists.
//!
//! # A lock and a notification, not a concurrent map
//!
//! A waiting commit must check *who holds this position* and go to sleep without
//! missing a report that lands between the two, or the report is missed until
//! the deadline. It registers for the next notification BEFORE it checks, so a
//! report landing after the check still wakes it. The check reads every voter of
//! one position under one lock, held for map lookups and never across an
//! `.await` or any I/O; the map is as large as the number of (log, follower)
//! pairs.
//!
//! The wait is asynchronous because a commit waits on a runtime worker, and the
//! follower's ask that ends the wait is answered by a task on the same runtime: a
//! wait that held its thread would leave the asks unanswered and every waiter to
//! its deadline once the waiters outnumber the workers.
//!
//! Not persisted, for [`crate::followers`]'s reason: a restarted leader knows
//! nothing about its followers until they ask again, and that is the true answer.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use tokio::sync::Notify;

use tessari_encoding::{LogId, NODE_ID_LEN};
use tessari_types::Sequence;

/// One follower's standing in one log.
#[derive(Debug, Clone, Copy, Default)]
struct Standing {
    /// The highest position this leader process has sent it.
    sent: Sequence,
    /// What its latest ask says it holds, no further than `sent`.
    held: Sequence,
}

/// The latest durable position each follower has stated, per log.
#[derive(Debug, Default)]
pub(crate) struct Holds {
    // A wait reads every voter of one position in one consistent look (see the header).
    // bgv-allow(shared-map-lock): the waiter's check spans several followers' entries at once.
    standing: Mutex<BTreeMap<(LogId, [u8; NODE_ID_LEN]), Standing>>,
    moved: Notify,
}

impl Holds {
    /// Record that this leader sent `node` the records of `log` through `through`.
    pub(crate) fn sent(&self, node: [u8; NODE_ID_LEN], log: LogId, through: Sequence) {
        let mut standing = self.standing.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = standing.entry((log, node)).or_default();
        entry.sent = entry.sent.max(through);
    }

    /// Record that `node` asked for `log` from just after `holds`, and wake every
    /// waiter — counting only what this leader sent it.
    pub(crate) fn asked(&self, node: [u8; NODE_ID_LEN], log: LogId, holds: Sequence) {
        {
            let mut standing = self.standing.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = standing.entry((log, node)).or_default();
            entry.held = holds.min(entry.sent);
        }
        self.moved.notify_waiters();
    }

    /// Wait until `needed` of `voters` hold `log` through `at`, or `deadline`.
    ///
    /// Answers the voters that hold it either way — the caller compares the
    /// count to what it needed, and a short answer names who did hold it, which
    /// is what a refusal must say. `needed` of zero answers at once. The wait
    /// holds no thread: it is suspended until a report or the deadline.
    pub(crate) async fn await_held(
        &self,
        log: LogId,
        at: Sequence,
        voters: &[[u8; NODE_ID_LEN]],
        needed: usize,
        deadline: Instant,
    ) -> Vec<[u8; NODE_ID_LEN]> {
        loop {
            // Registered before the check, so a report landing after it still
            // wakes this wait (`notify_waiters` reaches every enabled waiter).
            let reported = self.moved.notified();
            let mut reported = std::pin::pin!(reported);
            reported.as_mut().enable();
            let holding: Vec<[u8; NODE_ID_LEN]> = {
                let standing = self.standing.lock().unwrap_or_else(PoisonError::into_inner);
                voters
                    .iter()
                    .filter(|node| {
                        standing
                            .get(&(log, **node))
                            .is_some_and(|reached| reached.held >= at)
                    })
                    .copied()
                    .collect()
            };
            if holding.len() >= needed || Instant::now() >= deadline {
                return holding;
            }
            // Run out or woken, the next turn checks again; at the deadline it
            // answers whoever holds the position by then.
            let _woken =
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), reported).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Holds;
    use std::time::{Duration, Instant};
    use tessari_encoding::{LogId, NODE_ID_LEN};
    use tessari_types::{NamespaceId, Reach, Sequence};

    const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
    const TWO: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
    const OUTSIDER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];

    fn line() -> LogId {
        LogId::line(Reach::Store)
    }

    /// A follower that was sent `log` through `at` and asked past it.
    fn holding(holds: &Holds, node: [u8; NODE_ID_LEN], log: LogId, at: Sequence) {
        holds.sent(node, log, at);
        holds.asked(node, log, at);
    }

    fn after(by: Duration) -> Instant {
        Instant::now().checked_add(by).expect("representable")
    }

    #[tokio::test]
    async fn a_position_held_by_enough_voters_answers_at_once_naming_them() {
        let holds = Holds::default();
        holding(&holds, ONE, line(), Sequence::new(7));
        holding(&holds, TWO, line(), Sequence::new(5));
        let began = Instant::now();
        assert_eq!(
            holds
                .await_held(
                    line(),
                    Sequence::new(5),
                    &[ONE, TWO],
                    2,
                    after(Duration::from_secs(5))
                )
                .await,
            vec![ONE, TWO]
        );
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "it waited for a deadline it did not need"
        );
    }

    #[tokio::test]
    async fn a_short_wait_ends_at_its_deadline_and_names_who_held_it() {
        let holds = Holds::default();
        holding(&holds, ONE, line(), Sequence::new(7));
        holding(&holds, TWO, line(), Sequence::new(4));
        assert_eq!(
            holds
                .await_held(
                    line(),
                    Sequence::new(5),
                    &[ONE, TWO],
                    2,
                    after(Duration::from_millis(20))
                )
                .await,
            vec![ONE],
            "a follower below the position counted toward it"
        );
    }

    #[tokio::test]
    async fn only_voters_count_and_only_in_the_log_asked_about() {
        let holds = Holds::default();
        holding(&holds, OUTSIDER, line(), Sequence::new(9));
        holding(
            &holds,
            ONE,
            LogId::line(Reach::Namespace(NamespaceId::new(1))),
            Sequence::new(9),
        );
        assert!(
            holds
                .await_held(
                    line(),
                    Sequence::new(5),
                    &[ONE, TWO],
                    1,
                    after(Duration::from_millis(20))
                )
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_follower_that_reports_less_no_longer_counts_its_old_mark() {
        // A follower that re-seeded after a fork asks from an earlier position.
        // Its earlier, higher report describes a copy that no longer exists.
        let holds = Holds::default();
        holding(&holds, ONE, line(), Sequence::new(9));
        holds.asked(ONE, line(), Sequence::new(2));
        assert!(
            holds
                .await_held(
                    line(),
                    Sequence::new(5),
                    &[ONE],
                    1,
                    after(Duration::from_millis(20))
                )
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_report_from_another_thread_wakes_the_waiter_before_its_deadline() {
        let holds = std::sync::Arc::new(Holds::default());
        let reporter = std::sync::Arc::clone(&holds);
        let reporting =
            std::thread::spawn(move || holding(&reporter, ONE, line(), Sequence::new(3)));
        let began = Instant::now();
        assert_eq!(
            holds
                .await_held(
                    line(),
                    Sequence::new(3),
                    &[ONE],
                    1,
                    after(Duration::from_secs(5))
                )
                .await,
            vec![ONE]
        );
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "the report did not wake the wait"
        );
        reporting.join().expect("the reporting thread");
    }

    #[tokio::test]
    async fn a_follower_counts_only_as_far_as_this_leader_sent_it() {
        // A copy finished by a deposed leadership asks from the same place a
        // faithful one does. Its claim counts only through what was sent here.
        let holds = Holds::default();
        holds.sent(ONE, line(), Sequence::new(3));
        holds.asked(ONE, line(), Sequence::new(9));
        let held = async |at| {
            holds
                .await_held(line(), Sequence::new(at), &[ONE], 1, Instant::now())
                .await
        };
        assert_eq!(held(3).await, vec![ONE]);
        assert!(
            held(4).await.is_empty(),
            "a position this leader never sent counted as held"
        );
    }

    #[test]
    fn a_waiting_commit_leaves_its_worker_to_the_report_it_waits_for() {
        // A commit waits on a runtime worker, and the follower's ask that ends
        // the wait is answered by a task on the same runtime. One worker makes
        // the starvation certain rather than a race: a wait that holds the
        // thread lets the report run only once the deadline has passed.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime");
        let holds = std::sync::Arc::new(Holds::default());
        let reporter = std::sync::Arc::clone(&holds);
        let began = Instant::now();
        let held = runtime.block_on(async move {
            let reporting = tokio::spawn(async move {
                holding(&reporter, ONE, line(), Sequence::new(3));
            });
            let held = holds
                .await_held(
                    line(),
                    Sequence::new(3),
                    &[ONE],
                    1,
                    after(Duration::from_secs(2)),
                )
                .await;
            reporting.await.expect("the reporting task");
            held
        });
        assert_eq!(held, vec![ONE], "the report never reached the wait");
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "the wait held the worker the report needed"
        );
    }
}
