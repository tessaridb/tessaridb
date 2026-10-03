//! Cluster events this process counts for its operator (G053 C6).
//!
//! Counters, not state: nothing reads them to decide anything, so they are
//! relaxed atomics and never persisted — a count that outlived the process that
//! observed it would be a claim nobody can check (the reasoning `Health` gives
//! for the divergence count beside them).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `NotHeldHere` answers and majority waits.
#[derive(Debug, Default)]
pub(crate) struct ClusterTally {
    not_held_here: AtomicU64,
    waits: AtomicU64,
    timeouts: AtomicU64,
    waited_micros: AtomicU64,
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ClusterTally;

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
}
