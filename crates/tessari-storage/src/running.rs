//! What this process is doing with the consumers the catalog declares.
//!
//! # Why none of this is persisted
//!
//! A declaration is durable and replicates; *whether a consumer is running* is
//! neither. It is not even durable **locally**, and that is the sharper half of
//! the rule: a node killed while consuming would leave `running: true` behind in
//! any keyspace that held it, and the next process to read it would report a
//! consumer that is not there. Persisted state about a running thread is a claim
//! that outlives the thread.
//!
//! So this lives in the process, is empty at open, and is filled by the runner
//! as it starts each consumer. A restarted node reports nothing running until it
//! has actually started something, which is the answer that is true.
//!
//! Positions are here for the same reason and one more: the **broker** owns the
//! authoritative offset for a group. What this records is where this process had
//! reached when it last committed, which is a diagnostic — the number to compare
//! against the broker's, not a number to resume from.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// How one consumer of a declaration is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// How many records this process has applied.
    pub applied: u64,
    /// How many messages it has parked because they could not be applied.
    pub quarantined: u64,
    /// The last failure, if there has been one.
    ///
    /// Kept even after the consumer recovers: an error that cleared itself is
    /// the one an operator most needs to see, because nothing else will ever
    /// mention it.
    pub last_error: Option<String>,
    /// Where this process had reached, by partition, when it last committed.
    pub positions: BTreeMap<i32, i64>,
}

/// The consumers this process is running, by declared name.
///
/// Shared by every handle to one store, so a session answering `INFO FOR
/// CONSUMER` sees what the runner's threads are doing rather than a copy of it.
#[derive(Debug, Default)]
pub struct Running {
    live: Mutex<BTreeMap<String, Progress>>,
}

impl Running {
    /// Mark a consumer as running, with nothing done yet.
    ///
    /// Replaces whatever was there: a consumer being started is a consumer
    /// whose previous run has ended, and carrying the old counters forward
    /// would report work this run did not do.
    pub fn started(&self, name: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.insert(name.to_owned(), Progress::default());
        }
    }

    /// Forget a consumer, because it has stopped.
    pub fn stopped(&self, name: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(name);
        }
    }

    /// Record what one batch did.
    ///
    /// A no-op when the consumer is not registered, rather than an insert: an
    /// update arriving after `stopped` is a thread finishing its last batch, and
    /// resurrecting the entry would report it as running.
    pub fn advanced(&self, name: &str, change: impl FnOnce(&mut Progress)) {
        if let Ok(mut live) = self.live.lock()
            && let Some(progress) = live.get_mut(name)
        {
            change(progress);
        }
    }

    /// How one consumer is doing, if this process is running it.
    #[must_use]
    pub fn progress(&self, name: &str) -> Option<Progress> {
        self.live.lock().ok()?.get(name).cloned()
    }

    /// Every consumer this process is running.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.live
            .lock()
            .map(|live| live.keys().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_consumer_that_was_never_started_is_not_running() {
        let running = Running::default();
        assert!(running.progress("orders_in").is_none());
        assert!(running.names().is_empty());
    }

    #[test]
    fn starting_a_second_time_does_not_carry_the_first_runs_counters() {
        // A restarted consumer reporting the previous run's totals would make
        // "applied" a number nobody can act on: it would answer a question about
        // the process that is running with a fact about one that is not.
        let running = Running::default();
        running.started("orders_in");
        running.advanced("orders_in", |progress| progress.applied = 9);
        running.started("orders_in");
        assert_eq!(running.progress("orders_in").unwrap().applied, 0);
    }

    #[test]
    fn a_batch_finishing_after_the_stop_does_not_resurrect_the_entry() {
        // The shutdown race, asserted rather than reasoned about: the runner
        // marks a consumer stopped and its thread then finishes the batch it was
        // already in. An `advanced` that inserted would report a consumer that
        // has gone.
        let running = Running::default();
        running.started("orders_in");
        running.stopped("orders_in");
        running.advanced("orders_in", |progress| progress.applied = 3);
        assert!(running.progress("orders_in").is_none());
    }

    #[test]
    fn the_last_error_survives_the_batch_that_succeeded_after_it() {
        // An error that cleared itself is the one nothing else will mention.
        let running = Running::default();
        running.started("orders_in");
        running.advanced("orders_in", |progress| {
            progress.last_error = Some("broker refused".to_owned());
        });
        running.advanced("orders_in", |progress| progress.applied = 1);
        let progress = running.progress("orders_in").unwrap();
        assert_eq!(progress.applied, 1);
        assert_eq!(progress.last_error.as_deref(), Some("broker refused"));
    }
}
