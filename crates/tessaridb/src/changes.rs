//! Reading a log's changes: from a position, to its tail, and as a subscription.

use super::*;

impl Db {
    /// What changed in one log from `from` onward, oldest first.
    ///
    /// A projection of the replication log: no state, no registration, and the
    /// same answer on a replica reading the same log. The result carries where
    /// to resume, because a commit that changed no records still moves a reader
    /// forward.
    ///
    /// `log` names which one, and it is not optional for the reason `from` is
    /// not: a position counts in one log and in no other, so a call that left
    /// the log to be assumed would be spending one counter's number against
    /// another's. It names a writer as well as a home, because a range that
    /// admits two masters has a log per writer and the home alone no longer
    /// picks one out.
    ///
    /// # Errors
    ///
    /// Returns an error when a record or a payload cannot be read.
    pub fn changes_since(&self, log: LogId, from: Sequence, limit: usize) -> Result<Changes> {
        Ok(self.store.changes_since(log, from, limit)?)
    }

    /// The position of the newest committed change in one log.
    ///
    /// A subscription on that log starting after this one sees only what
    /// happens next.
    ///
    /// # Errors
    ///
    /// Returns an error when the position cannot be read.
    pub fn committed_tail(&self, log: LogId) -> Result<Sequence> {
        Ok(self.store.committed_tail(log)?)
    }

    /// Follow the changes to one table, or to all of them, from `from` onward.
    ///
    /// The subscription is a value you keep. It holds a position and two
    /// counters and nothing else, so this database has no registry of
    /// subscribers: nothing to leak, nothing to clean up when a caller
    /// disappears, and no lock on the write path.
    #[must_use]
    pub const fn subscribe(log: LogId, from: Sequence, watch: Watch) -> Subscription {
        Subscription::new(log, from, watch)
    }

    /// The next changes a subscription is waiting for, advancing it.
    ///
    /// Advances over every record read, not only over the ones that matched, so
    /// a subscription watching one table does not stall on a run of writes to
    /// another.
    ///
    /// # Errors
    ///
    /// Returns an error when a record or a payload cannot be read.
    pub fn poll(&self, subscription: &mut Subscription, limit: usize) -> Result<Vec<Change>> {
        Ok(subscription.poll(&self.store, limit)?)
    }

    /// Give up on a subscription's backlog below `target`, counting the loss.
    ///
    /// For a subscriber that would rather be current than complete. Returns how
    /// many watched changes were discarded — exactly, because the skip reads
    /// what it discards, and an approximate loss figure is one nobody can act
    /// on.
    ///
    /// # Errors
    ///
    /// Returns an error when a record or a payload cannot be read.
    pub fn skip(&self, subscription: &mut Subscription, target: Sequence) -> Result<u64> {
        Ok(subscription.skip_to(&self.store, target)?)
    }
}
