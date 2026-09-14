//! Reading what changed, out of the log that already records it.
//!
//! # Derived, not built
//!
//! Every commit writes a log record carrying exactly what it changed, in address
//! order, and that record has been carrying replication since SG2.T7. A change
//! feed is a **projection of it** rather than a second mechanism — so it needs
//! no state of its own, cannot disagree with what was committed, and a replica's
//! feed over the same log is identical to the leader's. That is the property
//! index maintenance and schema validation already have, and it is why both are
//! computed from the log rather than transmitted alongside it.
//!
//! # A change says what a record became, not what it was
//!
//! Two kinds, `Written` and `Removed` — not created, updated and deleted. The
//! log carries the new state, so calling a write a *creation* means knowing what
//! stood there at that sequence, which is MVCC history rather than the current
//! state. A feed that consulted the current state instead would label a record
//! created and then changed as an update, and **a wrong label is worse than a
//! missing one**: a subscriber can tell new from changed by keeping its own set,
//! and cannot recover from being told the wrong thing.
//!
//! # The catalog is not in the feed
//!
//! A catalog entry is an ordinary record in the system tenancy (ADR-0009), which
//! is exactly what makes replication and index maintenance work and exactly what
//! a subscriber watching `users` does not want. Schema evolution is a different
//! feed with a different shape; conflating them would put rows nobody asked for
//! into every subscription.

use tessari_constants::SKIP_BATCH_RECORDS;
use tessari_encoding::{LogRecord, RecordValue, decode_payload};
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, TableId, Value};

use crate::catalog::{SYSTEM_DATABASE, SYSTEM_NAMESPACE};
use crate::error::Result;
use crate::store::Store;

/// What a read of the feed found, and where to resume.
///
/// The position is not a convenience. A log record can produce **no changes** —
/// a commit that only touched the catalog does exactly that — so a subscriber
/// given only a list cannot tell "nothing has happened" from "nothing I care
/// about has happened", and would ask for the same records forever. Returning
/// where the read reached is what lets it advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// What changed, oldest first, in the log's own order.
    pub changes: Vec<Change>,
    /// The sequence to ask from next.
    ///
    /// One past the last record read, or the sequence asked for when there was
    /// nothing to read — so passing it back always means "whatever is new".
    pub next: Sequence,
}

/// What happened to one record, at one sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The commit this change was part of.
    ///
    /// Shared by every change of the same commit, which is what lets a
    /// subscriber apply them as the unit they were written as.
    pub sequence: Sequence,
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within it.
    pub database: DatabaseId,
    /// The table within that.
    pub table: TableId,
    /// The record's identity.
    pub id: RecordId,
    /// What became of it.
    pub kind: ChangeKind,
}

/// What became of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// The record now holds this value.
    ///
    /// Whether that is its first value or its fifth is not said, because the log
    /// does not carry it — see the module documentation.
    Written(Value),
    /// The record is no longer there.
    Removed,
}

/// The changes one log record carries, in the order the log carries them.
///
/// # Errors
///
/// Returns a decoding failure when a payload cannot be read. A payload that
/// cannot be decoded is corruption, not a change to skip: skipping it would give
/// a subscriber a feed that silently disagrees with the store.
pub(crate) fn changes_in(sequence: Sequence, record: &LogRecord) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for mutation in record.mutations() {
        if mutation.namespace == SYSTEM_NAMESPACE && mutation.database == SYSTEM_DATABASE {
            continue;
        }
        let kind = match &mutation.value {
            RecordValue::Present(payload) => ChangeKind::Written(decode_payload(payload)?),
            RecordValue::Tombstone => ChangeKind::Removed,
        };
        changes.push(Change {
            sequence,
            namespace: mutation.namespace,
            database: mutation.database,
            table: mutation.table,
            id: mutation.id.clone(),
            kind,
        });
    }
    Ok(changes)
}

/// Which changes a subscriber is watching for.
///
/// Inside the subscription rather than applied by the caller afterwards, because
/// the filter is part of what "emitted" *means* for that subscriber: one
/// watching `users` should not have its loss account inflated by every write to
/// every other table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Watch {
    /// The table to watch, or every table when absent.
    pub table: Option<TableId>,
}

impl Watch {
    /// Watch one table.
    #[must_use]
    pub const fn table(table: TableId) -> Self {
        Self { table: Some(table) }
    }

    /// Whether this change is one the subscriber asked for.
    #[must_use]
    pub fn covers(self, change: &Change) -> bool {
        self.table.is_none_or(|table| table == change.table)
    }
}

/// A durable cursor over the feed, and what it has and has not seen.
///
/// # Why a cursor rather than a queue
///
/// The usual subscription is a bounded in-memory queue per subscriber, filled at
/// commit time and dropping the oldest when it overflows. This store does not do
/// that, because it **already has a better buffer**: the log is durable, ordered
/// and unbounded, so a subscriber that keeps a position cannot lose anything by
/// being slow — it can only be behind. A queue in front of it would be a second,
/// worse buffer whose drops are caused by memory pressure rather than by any
/// decision anyone made.
///
/// The consequence is that the store holds **no registry of subscribers**: a
/// subscription is a value the caller holds and can persist, so there is nothing
/// to leak, nothing to clean up when a client disappears, and no lock on the
/// commit path. Its position is a [`Sequence`], so a restarted process — or a
/// replica — resumes from exactly where it stopped.
///
/// # The loss account, and why it is not vacuous
///
/// If nothing can ever be lost, `dropped` is always zero and "no loss under
/// backlog" is satisfied by saying nothing. So the one operation that *can* lose
/// is [`Subscription::skip_to`], for a subscriber that has decided it would
/// rather be current than complete — and it counts exactly what it skips, by
/// reading it. **A skip is the only way to lose a change, and it is always the
/// subscriber's own decision.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subscription {
    home: Reach,
    position: Sequence,
    watch: Watch,
    delivered: u64,
    dropped: u64,
}

impl Subscription {
    /// Watch one home's log from a position onward.
    ///
    /// The home is part of the cursor rather than an argument to each read: a
    /// position counts in one log, so a subscription that could be polled
    /// against a different home each time would be carrying a number from one
    /// counter and spending it against another.
    #[must_use]
    pub const fn new(home: Reach, from: Sequence, watch: Watch) -> Self {
        Self {
            home,
            position: from,
            watch,
            delivered: 0,
            dropped: 0,
        }
    }

    /// The log this subscription reads.
    #[must_use]
    pub const fn home(self) -> Reach {
        self.home
    }

    /// Where the next read will start.
    #[must_use]
    pub const fn position(self) -> Sequence {
        self.position
    }

    /// How many changes this subscription has been given.
    #[must_use]
    pub const fn delivered(self) -> u64 {
        self.delivered
    }

    /// How many it skipped past without being given.
    #[must_use]
    pub const fn dropped(self) -> u64 {
        self.dropped
    }
}

impl Subscription {
    /// The next changes this subscription is watching for.
    ///
    /// Advances over every record it **read**, not only over the ones that
    /// matched — a subscriber watching one table would otherwise stall on a run
    /// of writes to another, asking for the same records forever. That is the
    /// same reason [`Changes`] carries a position at all.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a record cannot be decoded.
    pub fn poll(&mut self, store: &Store, limit: usize) -> Result<Vec<Change>> {
        let answer = store.changes_since(self.home, self.position, limit)?;
        let watch = self.watch;
        let matched: Vec<Change> = answer
            .changes
            .into_iter()
            .filter(|change| watch.covers(change))
            .collect();
        self.position = answer.next;
        self.delivered = self
            .delivered
            .saturating_add(matched.len().try_into().unwrap_or(u64::MAX));
        Ok(matched)
    }

    /// Give up on the backlog below `target`, counting exactly what is lost.
    ///
    /// For a subscriber that has decided it would rather be current than
    /// complete. The count is exact because it is taken by reading what is
    /// discarded: an approximate loss figure is one nobody can act on, and
    /// reading to count is still far cheaper than delivering.
    ///
    /// Returns how many watched changes were skipped. A target at or behind the
    /// current position does nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a record cannot be decoded.
    pub fn skip_to(&mut self, store: &Store, target: Sequence) -> Result<u64> {
        let mut skipped = 0_u64;
        while self.position < target {
            let answer = store.changes_since(self.home, self.position, SKIP_BATCH_RECORDS)?;
            if answer.next == self.position {
                // Nothing left in the log: the target is beyond its end, and the
                // subscriber has skipped everything there was to skip.
                break;
            }
            for change in &answer.changes {
                if change.sequence < target && self.watch.covers(change) {
                    skipped = skipped.saturating_add(1);
                }
            }
            self.position = answer.next.min(target);
        }
        self.dropped = self.dropped.saturating_add(skipped);
        Ok(skipped)
    }
}
