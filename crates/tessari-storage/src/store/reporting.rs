//! Health, followers and collection, as the node reports them.

use std::sync::atomic::Ordering;

use tessari_encoding::{LogId, NODE_ID_LEN};
use tessari_types::Sequence;

use crate::catalog::Reach;
use crate::error::Result;
use crate::followers::FollowerLag;

use super::{Health, Store, UNPARTITIONED_REPORT_HOME};

impl Store {
    /// Record that this node stood in a leadership round.
    ///
    /// Called by the campaign cadence and by nothing else. It is a store method
    /// rather than a counter in the serving process for the reason
    /// [`Health::log_divergences`] is one: the scrape reads the store's health,
    /// so a detector that lives anywhere else is a detector an operator cannot
    /// see.
    pub fn campaigned(&self) {
        self.campaigns.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether this store is well, and what is wrong when it is not.
    ///
    /// # Why this exists rather than a metric
    ///
    /// An engine does its compaction, its flushing and its write-ahead work on
    /// its own threads, and a failure there surfaces at **no call a caller
    /// makes**. The store keeps answering reads while the thing that keeps it
    /// durable has stopped. That is the one failure this store cannot detect by
    /// being used, so something has to ask.
    ///
    /// # Where the alert lives, and why it is not here
    ///
    /// Not here. This answers *what is true*; deciding it is worth waking
    /// somebody for belongs to whatever already wakes people. The HTTP surface
    /// turns an unwell store into a failing `GET /health`, which every load
    /// balancer takes out of rotation and every monitor pages on — so the alert
    /// is the one that already exists rather than a second one written here and
    /// tested never.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the counts cannot be read.
    pub fn health(&self) -> Result<Health> {
        // The log this node's history on the line is in (ADR-0107): the line's
        // once a leadership has written it, its own on a store standing alone.
        let own = self.history_log(UNPARTITIONED_REPORT_HOME)?;
        let (across_committed, across_aborted, across_in_doubt) = self.tally.outcomes();
        let (across_pending, across_with_intents) = self.tally.standing();
        Ok(Health {
            background_errors: self.backend.background_errors()?,
            committed: self.committed_tail(own)?,
            elsewhere: self.furthest_other_log(UNPARTITIONED_REPORT_HOME, own)?,
            log_divergences: self.divergences.load(Ordering::Relaxed),
            discarded_writes: self.discarded.load(Ordering::Relaxed),
            campaigns: self.campaigns.load(Ordering::Relaxed),
            lease_remaining: self.lease.remaining(),
            not_held_here: self.tally.not_held_here(),
            acknowledgement_waits: self.tally.waits(),
            acknowledgement_timeouts: self.tally.timeouts(),
            acknowledgement_waited: self.tally.waited(),
            across_committed,
            across_aborted,
            across_in_doubt,
            across_pending,
            across_with_intents,
        })
    }

    /// Record how one transaction across leaders this node coordinated ended
    /// for its client. Called where the driver answers, and nowhere else.
    pub fn across_finished(&self, outcome: crate::tally::AcrossOutcome) {
        self.tally.finished(outcome);
    }

    /// Record what a settling pass left standing here: records still
    /// `PENDING`, and transactions still holding intents.
    pub fn across_sampled(&self, pending: u64, with_intents: u64) {
        self.tally.sampled(pending, with_intents);
    }

    /// Record one read that reached this node holding none of what it asked
    /// for. Called where the `NotHeldHere` refusal is made, and nowhere else.
    pub fn answered_not_held_here(&self) {
        self.tally.held_elsewhere();
    }

    /// Record one commit that waited `waited` for a majority, and whether it
    /// ran out of time. Called where the wait happens, and nowhere else.
    pub fn acknowledgement_waited(&self, waited: std::time::Duration, timed_out: bool) {
        self.tally.waited_for(waited, timed_out);
    }

    /// Record what a follower has been given.
    ///
    /// # Why the store holds this and not the door
    ///
    /// The door — `Session::replicate_from` — is where a follower names itself
    /// and where the read happens, so it is where the call is made. But the
    /// registry belongs to the store, because every handle to one store is one
    /// leader: a follower recorded against a second handle is a follower the
    /// first one would report as never having collected.
    pub fn follower_served(&self, node: [u8; NODE_ID_LEN], home: Reach, reached: Sequence) {
        self.followers.served(node, home, reached);
    }

    /// Record that this leader sent `node` the records of `log` through
    /// `through` — the bound on what that follower's next ask can vouch for.
    pub fn follower_sent(&self, node: [u8; NODE_ID_LEN], log: LogId, through: Sequence) {
        self.holds.sent(node, log, through);
    }

    /// Record that `node` asked for `log` from just after `holds` — what it has
    /// made durable, counted only as far as this leader sent it (ADR-0106 D6).
    ///
    /// The latest ask replaces the last, so a follower that re-seeded and asks
    /// from an earlier position stops counting toward a majority it no longer
    /// belongs to.
    pub fn follower_asked(&self, node: [u8; NODE_ID_LEN], log: LogId, holds: Sequence) {
        self.holds.asked(node, log, holds);
    }

    /// Wait until `needed` of `voters` hold `log` through `at`, for at most
    /// `within`, and answer the voters that do.
    ///
    /// Blocks the calling thread, so it is called off the async runtime — where
    /// a commit already runs. The answer is the same shape whether the wait was
    /// met or ran out, because a refusal names who held the write.
    #[must_use]
    pub fn await_held(
        &self,
        log: LogId,
        at: Sequence,
        voters: &[[u8; NODE_ID_LEN]],
        needed: usize,
        within: std::time::Duration,
    ) -> Vec<[u8; NODE_ID_LEN]> {
        let deadline = std::time::Instant::now()
            .checked_add(within)
            .unwrap_or_else(std::time::Instant::now);
        self.holds.await_held(log, at, voters, needed, deadline)
    }

    /// Record what this node collected for itself, and whether it arrived.
    ///
    /// The follower's twin of [`Self::follower_served`], and the only way
    /// [`Self::current_as_of`] ever answers anything but `None` on a node that
    /// may not write. `currency` is the caller's observation and not a
    /// judgement: a collection whose answer was shorter than the bound it named
    /// is [`Currency::Level`], because a peer serves `min(limit, available)` and
    /// a short answer means it had no more.
    pub fn collected(&self, reached: Sequence, currency: crate::collections::Currency) {
        self.collections.collected(reached, currency);
    }

    /// The last collection this node made for itself, if it has made one.
    #[must_use]
    pub fn collection(&self) -> Option<crate::collections::Collection> {
        self.collections.last()
    }

    /// Record where this node now stands against the peer it collects from.
    ///
    /// Set by the node's collection round when it starts a copy, when one
    /// fails, and when it finds itself stranded; a collection that lands sets
    /// it through [`Self::collected`].
    pub fn upstream_is(&self, state: crate::collections::Upstream) {
        self.collections.upstream_is(state);
    }

    /// Record a copy of the leader's state that installed `records` records.
    pub fn replica_copied(&self, records: u64) {
        self.collections.copied(records);
    }

    /// Where this node stands against its upstream, or `None` on a node that
    /// has never collected nor copied.
    #[must_use]
    pub fn upstream(&self) -> Option<crate::collections::UpstreamReport> {
        self.collections.upstream()
    }

    /// How far behind every follower this process has served is.
    ///
    /// Measured against this leader's own committed tail, from what it handed
    /// out — no connection to the follower is opened, and none is needed,
    /// because the leader served every byte the follower holds.
    ///
    /// A follower that has never collected is **absent** from this list rather
    /// than present at zero. `INFO FOR NODE` draws the same distinction one
    /// level up between a node no membership row names and one whose row names
    /// no roles.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the committed tail cannot be read.
    pub fn follower_lag(&self) -> Result<Vec<FollowerLag>> {
        let mut rows = Vec::new();
        for (node, held) in self.followers.seen() {
            // The tail of the follower's OWN log. Reading one log's tail against
            // a position taken in another is not an approximation — the two
            // counters are unrelated and the subtraction is meaningless (Q-622).
            // The log this leader serves, which is the one the position was
            // taken in (ADR-0107).
            let tail = self.committed_tail(self.history_log(held.home)?)?;
            rows.push(FollowerLag {
                node,
                home: held.home,
                sequence: held.sequence,
                behind: tail.get().saturating_sub(held.sequence.get()),
                quiet_for: held.at.elapsed(),
                // Only for the log the tail marks actually sample. For any other
                // log there is no timeline to read the position against, and
                // `None` — beyond every bound — is the honest answer.
                copy_age: self.tailmarks.age_of(held.home, held.sequence),
            });
        }
        Ok(rows)
    }

    /// Date this leader's own committed tail, as of now.
    ///
    /// Called once per awareness interval by the node binary, and by nothing
    /// else. It is what gives [`crate::FollowerLag::copy_age`] a timeline to be
    /// read against; a leader that never calls it reports every follower's copy
    /// age as unknown, which is the honest answer for a leader that has never
    /// dated anything.
    ///
    /// Deliberately **not** on the commit path. Sampling where the tail actually
    /// moves would be exact and would put a lock on the hottest path in the
    /// engine for the sake of a diagnostic; the cadence already runs and already
    /// opens the store. What that costs is precision, bounded at one interval
    /// and always in the safe direction — see [`crate::tailmarks`].
    ///
    /// # Errors
    ///
    /// Returns the backend's failure when the committed tail cannot be read.
    pub fn mark_tail(&self, home: Reach) -> Result<()> {
        // This node's OWN log for the home, which is what a diagnostic asking
        // *how far is this range* has always meant here. A home with two
        // writers has a second tail this mark does not carry, and reporting it
        // is Q-622's, not this diagnostic's.
        let log = self.history_log(home)?;
        self.tailmarks.mark(home, self.committed_tail(log)?);
        Ok(())
    }
}
