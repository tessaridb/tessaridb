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
//! # A lock and a condition, not a concurrent map
//!
//! A waiting commit must check *who holds this position* and sleep in one step,
//! or a report landing between the two is missed until the deadline. That is the
//! wait-notify shape, and it needs the one lock the condition is checked under —
//! which a sharded map cannot be. The lock is held for a map lookup and never
//! across I/O; the map is as large as the number of (log, follower) pairs.
//!
//! Not persisted, for [`crate::followers`]'s reason: a restarted leader knows
//! nothing about its followers until they ask again, and that is the true answer.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

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
    // The wait must check the condition and sleep under one lock (see the header).
    // bgv-allow(shared-map-lock): a Condvar waits on this guard; no sharded map offers one.
    standing: Mutex<BTreeMap<(LogId, [u8; NODE_ID_LEN]), Standing>>,
    moved: Condvar,
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
        self.moved.notify_all();
    }

    /// Wait until `needed` of `voters` hold `log` through `at`, or `deadline`.
    ///
    /// Answers the voters that hold it either way — the caller compares the
    /// count to what it needed, and a short answer names who did hold it, which
    /// is what a refusal must say. `needed` of zero answers at once.
    pub(crate) fn await_held(
        &self,
        log: LogId,
        at: Sequence,
        voters: &[[u8; NODE_ID_LEN]],
        needed: usize,
        deadline: Instant,
    ) -> Vec<[u8; NODE_ID_LEN]> {
        let mut standing = self.standing.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let holding: Vec<[u8; NODE_ID_LEN]> = voters
                .iter()
                .filter(|node| {
                    standing
                        .get(&(log, **node))
                        .is_some_and(|reached| reached.held >= at)
                })
                .copied()
                .collect();
            let now = Instant::now();
            if holding.len() >= needed || now >= deadline {
                return holding;
            }
            standing = self
                .moved
                .wait_timeout(standing, deadline.saturating_duration_since(now))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
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

    #[test]
    fn a_position_held_by_enough_voters_answers_at_once_naming_them() {
        let holds = Holds::default();
        holding(&holds, ONE, line(), Sequence::new(7));
        holding(&holds, TWO, line(), Sequence::new(5));
        let began = Instant::now();
        assert_eq!(
            holds.await_held(
                line(),
                Sequence::new(5),
                &[ONE, TWO],
                2,
                after(Duration::from_secs(5))
            ),
            vec![ONE, TWO]
        );
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "it waited for a deadline it did not need"
        );
    }

    #[test]
    fn a_short_wait_ends_at_its_deadline_and_names_who_held_it() {
        let holds = Holds::default();
        holding(&holds, ONE, line(), Sequence::new(7));
        holding(&holds, TWO, line(), Sequence::new(4));
        assert_eq!(
            holds.await_held(
                line(),
                Sequence::new(5),
                &[ONE, TWO],
                2,
                after(Duration::from_millis(20))
            ),
            vec![ONE],
            "a follower below the position counted toward it"
        );
    }

    #[test]
    fn only_voters_count_and_only_in_the_log_asked_about() {
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
                .is_empty()
        );
    }

    #[test]
    fn a_follower_that_reports_less_no_longer_counts_its_old_mark() {
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
                .is_empty()
        );
    }

    #[test]
    fn a_report_from_another_thread_wakes_the_waiter_before_its_deadline() {
        let holds = std::sync::Arc::new(Holds::default());
        let reporter = std::sync::Arc::clone(&holds);
        let reporting =
            std::thread::spawn(move || holding(&reporter, ONE, line(), Sequence::new(3)));
        let began = Instant::now();
        assert_eq!(
            holds.await_held(
                line(),
                Sequence::new(3),
                &[ONE],
                1,
                after(Duration::from_secs(5))
            ),
            vec![ONE]
        );
        assert!(
            began.elapsed() < Duration::from_secs(4),
            "the report did not wake the wait"
        );
        reporting.join().expect("the reporting thread");
    }

    #[test]
    fn a_follower_counts_only_as_far_as_this_leader_sent_it() {
        // A copy finished by a deposed leadership asks from the same place a
        // faithful one does. Its claim counts only through what was sent here.
        let holds = Holds::default();
        holds.sent(ONE, line(), Sequence::new(3));
        holds.asked(ONE, line(), Sequence::new(9));
        let held = |at| holds.await_held(line(), Sequence::new(at), &[ONE], 1, Instant::now());
        assert_eq!(held(3), vec![ONE]);
        assert!(
            held(4).is_empty(),
            "a position this leader never sent counted as held"
        );
    }
}
