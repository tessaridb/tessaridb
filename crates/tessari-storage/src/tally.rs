//! Cluster events this process counts for its operator (G053 C6).
//!
//! Counters, not state: nothing reads them to decide anything, so they are
//! relaxed atomics and never persisted — a count that outlived the process that
//! observed it would be a claim nobody can check (the reasoning `Health` gives
//! for the divergence count beside them).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How a transaction across leaders ended for the client that asked for it
/// (ADR-0112 D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcrossOutcome {
    /// Committed, and a majority holds the decision.
    Committed,
    /// Nothing of it was committed.
    Aborted,
    /// The decision was sent and not confirmed.
    InDoubt,
}

/// What a sample has not been taken of yet: a gauge nobody has measured is not
/// a gauge reading zero.
const UNSAMPLED: u64 = u64::MAX;

/// `NotHeldHere` answers, majority waits, and transactions across leaders.
#[derive(Debug)]
pub(crate) struct ClusterTally {
    not_held_here: AtomicU64,
    waits: AtomicU64,
    timeouts: AtomicU64,
    waited_micros: AtomicU64,
    across_committed: AtomicU64,
    across_aborted: AtomicU64,
    across_in_doubt: AtomicU64,
    /// Records `PENDING` here, as the last settling pass left them.
    across_pending: AtomicU64,
    /// Transactions with intents standing here, as the last pass left them.
    across_with_intents: AtomicU64,
}

impl Default for ClusterTally {
    fn default() -> Self {
        Self {
            not_held_here: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            waited_micros: AtomicU64::new(0),
            across_committed: AtomicU64::new(0),
            across_aborted: AtomicU64::new(0),
            across_in_doubt: AtomicU64::new(0),
            across_pending: AtomicU64::new(UNSAMPLED),
            across_with_intents: AtomicU64::new(UNSAMPLED),
        }
    }
}

impl ClusterTally {
    pub(crate) fn held_elsewhere(&self) {
        self.not_held_here.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn waited_for(&self, waited: Duration, timed_out: bool) {
        self.waits.fetch_add(1, Ordering::Relaxed);
        if timed_out {
            self.timeouts.fetch_add(1, Ordering::Relaxed);
        }
        let micros = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
        self.waited_micros.fetch_add(micros, Ordering::Relaxed);
    }

    pub(crate) fn not_held_here(&self) -> u64 {
        self.not_held_here.load(Ordering::Relaxed)
    }

    pub(crate) fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    pub(crate) fn timeouts(&self) -> u64 {
        self.timeouts.load(Ordering::Relaxed)
    }

    pub(crate) fn waited(&self) -> Duration {
        Duration::from_micros(self.waited_micros.load(Ordering::Relaxed))
    }

    pub(crate) fn finished(&self, outcome: AcrossOutcome) {
        let counter = match outcome {
            AcrossOutcome::Committed => &self.across_committed,
            AcrossOutcome::Aborted => &self.across_aborted,
            AcrossOutcome::InDoubt => &self.across_in_doubt,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// How many transactions ended each way: committed, aborted, in doubt.
    pub(crate) fn outcomes(&self) -> (u64, u64, u64) {
        (
            self.across_committed.load(Ordering::Relaxed),
            self.across_aborted.load(Ordering::Relaxed),
            self.across_in_doubt.load(Ordering::Relaxed),
        )
    }

    /// Replace the sampled gauges with what a settling pass left standing.
    pub(crate) fn sampled(&self, pending: u64, with_intents: u64) {
        // One below the marker at most, so a count can never read as unsampled.
        self.across_pending
            .store(pending.min(UNSAMPLED - 1), Ordering::Relaxed);
        self.across_with_intents
            .store(with_intents.min(UNSAMPLED - 1), Ordering::Relaxed);
    }

    /// The sampled gauges, `None` before the first pass.
    pub(crate) fn standing(&self) -> (Option<u64>, Option<u64>) {
        let read = |gauge: &AtomicU64| {
            let held = gauge.load(Ordering::Relaxed);
            (held != UNSAMPLED).then_some(held)
        };
        (read(&self.across_pending), read(&self.across_with_intents))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AcrossOutcome, ClusterTally};

    #[test]
    fn a_wait_counts_once_and_a_timeout_also_counts_as_a_wait() {
        let tally = ClusterTally::default();
        tally.waited_for(Duration::from_millis(3), false);
        tally.waited_for(Duration::from_millis(200), true);
        tally.held_elsewhere();
        assert_eq!(tally.waits(), 2);
        assert_eq!(tally.timeouts(), 1);
        assert_eq!(tally.waited(), Duration::from_millis(203));
        assert_eq!(tally.not_held_here(), 1);
    }

    #[test]
    fn outcomes_count_apart_and_a_gauge_is_absent_until_sampled() {
        let tally = ClusterTally::default();
        assert_eq!(tally.standing(), (None, None));
        for outcome in [
            AcrossOutcome::Committed,
            AcrossOutcome::Committed,
            AcrossOutcome::Aborted,
            AcrossOutcome::InDoubt,
        ] {
            tally.finished(outcome);
        }
        assert_eq!(tally.outcomes(), (2, 1, 1));
        tally.sampled(0, 3);
        assert_eq!(tally.standing(), (Some(0), Some(3)));
        tally.sampled(u64::MAX, 0);
        assert_eq!(tally.standing(), (Some(u64::MAX - 1), Some(0)));
    }
}
