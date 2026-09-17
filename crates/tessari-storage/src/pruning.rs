//! Removing log records below a position nothing still needs.
//!
//! # Why the log needs its own answer, next to `reclaim`
//!
//! [`crate::reclaim`] removes record **versions** no live reader can resolve to.
//! This removes log **records**, and the two are not the same act in a different
//! keyspace: a version is a value somebody might read, while a log record is the
//! only account of how the state got where it is. Four subsystems replay it —
//! backup and restore, replication bootstrap, the change feed addressed by
//! position, and `INFO FOR HISTORY OF` — so removing one is a decision about
//! which of the four is allowed to lose, and not a cleanup.
//!
//! # The two acts, and why they are deliberately not one
//!
//! A prune advances a **start** and then removes the bytes below it. Those cannot
//! be one batch: a range delete is not an operation a [`tessari_kv::WriteBatch`]
//! carries, deliberately (see [`tessari_kv::KvBackend::delete_range`]).
//!
//! So the order is the safety argument, and it is the same argument `reclaim`
//! makes with the opposite conclusion. `reclaim` puts the floor **in** the batch
//! with the removals because a floor written afterwards is a floor a crash can
//! lose, *"leaving a store that has forgotten history and does not know it"*.
//! Here the two acts cannot be fused, so the rule is to put first whichever act
//! is harmful to lose:
//!
//! - **start, then bytes** — a crash in between leaves records below the start.
//!   They are unreachable, because every read below the start is refused, so
//!   nothing answers differently and the next pass removes them.
//! - bytes, then start — a crash in between leaves a hole under a start that
//!   still claims it. A reader walks into the missing span and the store reports
//!   [`crate::Error::LogGap`], which is this engine's word for *two histories
//!   have parted*. A cleanup would be raising a divergence alarm.
//!
//! Kafka orders it the same way and for the same reason: the log start offset
//! advances, and the segments go afterwards.
//!
//! # What is NOT here
//!
//! No policy and no schedule, which is the stance [`crate::reclaim`] takes in
//! its own words: *"when to call this is an operational decision that wants a
//! measurement, not a constant chosen while writing the code."* What may be
//! pruned — the floor computed from every consumer the store can see — is a
//! separate decision with separate evidence, and it is the thing that must never
//! be guessed.

use tessari_encoding::{LogId, LogKey, LogRetentionKey, LogStartKey, StoreKey, StoreValue};
use tessari_kv::{KeyRange, WriteBatch};
use tessari_types::Sequence;

use crate::error::Result;
use crate::store::Store;

/// What one prune removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pruned {
    /// How many records were removed.
    ///
    /// Arithmetic rather than counted, and exact because a log is gap-free by
    /// construction — [`Store::apply_record`] refuses a record that is not the
    /// next one, so there is no sparse span for a count to disagree about.
    pub records: u64,
    /// The oldest sequence this log holds now.
    pub start: Sequence,
}

impl Store {
    /// The oldest sequence one log still holds.
    ///
    /// [`Sequence::ZERO`] when nothing has ever been pruned from it, which is
    /// every log until a retention policy runs. Absence means **whole**, not
    /// empty: a log that has never been pruned begins wherever it begins.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored start cannot be
    /// decoded.
    pub fn log_start(&self, log: LogId) -> Result<Sequence> {
        let key = LogStartKey::new(log).encode();
        match self.backend().get(LogStartKey::keyspace(), &key)? {
            Some(value) => Ok(Sequence::decode(value.as_slice())?),
            None => Ok(Sequence::ZERO),
        }
    }

    /// Remove every record of one log at or below `upto`.
    ///
    /// # The last record always survives
    ///
    /// `upto` is **clamped** to one below the committed tail, and the clamp is
    /// not politeness. A follower that is level asks for `tail + 1`, and the
    /// leader answers it by reading the record **before** that position to state
    /// the leadership it follows (ADR-0059). Prune the tail record away and a
    /// perfectly healthy follower can no longer be served — the cluster stops
    /// replicating at the moment it has nothing to replicate.
    ///
    /// Clamping rather than refusing, because the caller above is a policy
    /// holding a target and this is the invariant: a policy that asked for more
    /// than the log can give should get what the log can give, and be told what
    /// that was. [`Pruned`] reports what actually happened.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored value cannot be
    /// decoded.
    pub fn prune_log(&self, log: LogId, upto: Sequence) -> Result<Pruned> {
        let tail = self.committed_tail(log)?;
        let held = self.log_start(log)?;
        // One below the tail, and never below zero.
        let ceiling = Sequence::new(tail.get().saturating_sub(1));
        let upto = upto.min(ceiling);
        if upto.get() == 0 || upto.get() < held.get() {
            return Ok(Pruned {
                records: 0,
                start: held,
            });
        }
        let start = Sequence::new(upto.get().saturating_add(1));

        // The start first. A crash after this and before the removal leaves
        // records nothing can read; a crash the other way round leaves a hole
        // under a start that still claims it.
        self.backend().apply(WriteBatch::new().put(
            LogStartKey::keyspace(),
            LogStartKey::new(log).encode(),
            start.encode(),
        ))?;

        // From below every real sequence rather than from the recorded start:
        // an interrupted earlier pass may have left records under it, and a
        // range that began at the start would walk past them forever.
        self.backend().delete_range(
            LogKey::keyspace(),
            &KeyRange::between(
                LogKey::new(log, Sequence::ZERO).encode(),
                LogKey::new(log, start).encode(),
            ),
        )?;

        Ok(Pruned {
            records: upto
                .get()
                .saturating_sub(held.get().max(1).saturating_sub(1)),
            start,
        })
    }
}

/// What one retention pass removed, across every log this node holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trimmed {
    /// How many logs were looked at.
    pub logs: usize,
    /// How many records were removed in total.
    pub records: u64,
}

impl Store {
    /// How many log records this node keeps, when it keeps a bounded number.
    ///
    /// `None` is **unbounded**, which is what every store held before retention
    /// existed and what every store holds until an operator sets a number. The
    /// default that changes nothing is the only safe default for an operation
    /// that cannot be undone.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored value cannot be
    /// decoded.
    pub fn log_retention(&self) -> Result<Option<Sequence>> {
        let key = LogRetentionKey.encode();
        match self.backend().get(LogRetentionKey::keyspace(), &key)? {
            Some(value) => Ok(Some(Sequence::decode(value.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Set — or clear — how many log records this node keeps.
    ///
    /// Written outside any transaction, because it is a fact about this machine
    /// rather than about the data, and the log does not carry it (ADR-0018). A
    /// retention that replicated would be inherited by whoever restored a backup
    /// and adopted by every follower of whoever set it.
    ///
    /// # Errors
    ///
    /// Returns the backend's own failure.
    pub fn set_log_retention(&self, keep: Option<Sequence>) -> Result<()> {
        let key = LogRetentionKey.encode();
        let batch = match keep {
            Some(keep) => WriteBatch::new().put(LogRetentionKey::keyspace(), key, keep.encode()),
            None => WriteBatch::new().delete(LogRetentionKey::keyspace(), key),
        };
        self.backend().apply(batch)?;
        Ok(())
    }

    /// Prune every log this node holds down to the retained record count.
    ///
    /// Answers `None` when no retention is set, which is not the same as
    /// answering zero: *nobody asked for this* and *there was nothing to do* are
    /// different facts, and a caller that logged the second every cadence would
    /// bury the first.
    ///
    /// # Which logs, and why all of them
    ///
    /// Every log this store holds, its own and the peers' copies alike. A
    /// follower's copy of a leader's log is this node's disk: nobody else reads
    /// it, and leaving it out would bound the disk of a leader and not of the
    /// nodes that carry the same records for it.
    ///
    /// # Who this can strand, stated rather than discovered
    ///
    /// The count is the whole bound. A follower inside the window is protected by
    /// being inside it; one that has fallen further behind than the window is
    /// **not** protected, and its next collect is refused with
    /// [`crate::Error::BelowLogStart`], which names the repair. That is Kafka's
    /// design and PostgreSQL's `max_slot_wal_keep_size` reaching its limit, and
    /// it is deliberate: the alternative — a floor held down by whichever reader
    /// is furthest behind — is the best-documented operational failure in this
    /// whole area, because one stuck reader then fills the disk and the node
    /// doing the stranding is in no error state at all.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored value cannot be
    /// decoded.
    pub fn trim_logs(&self) -> Result<Option<Trimmed>> {
        let Some(keep) = self.log_retention()? else {
            return Ok(None);
        };
        let mut trimmed = Trimmed::default();
        for log in self.logs()? {
            trimmed.logs = trimmed.logs.saturating_add(1);
            let tail = self.committed_tail(log)?;
            // Everything at or below this is older than the window. A tail that
            // has not yet reached the window subtracts to zero, and `prune_log`
            // treats zero as *nothing to do*.
            let upto = Sequence::new(tail.get().saturating_sub(keep.get()));
            let pruned = self.prune_log(log, upto)?;
            trimmed.records = trimmed.records.saturating_add(pruned.records);
        }
        Ok(Some(trimmed))
    }
}
