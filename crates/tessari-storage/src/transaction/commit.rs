//! Writing, committing, and losing the race.
//!
//! A commit is the only place this type touches the store, and the only place a
//! conflict can be raised. The backoff below is what a loser does before trying
//! again: randomised, so two threads that lost the same race do not re-enter it
//! together.

mod conflicts;
#[cfg(test)]
mod moved_map;
mod ranges;
mod settle;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tessari_constants::{COMMIT_BACKOFF_CEILING, COMMIT_BACKOFF_STEP, MAX_COMMIT_ATTEMPTS};
use tessari_encoding::{CausalStamp, LogId, LogRecord, Mutation, RecordValue, StampedValue};
use tessari_types::{Epoch, NamespaceId, Reach, Sequence, ShardId, TableId};

use super::{RecordAddress, Transaction};
use crate::catalog::ShardMap;
use crate::error::{ConflictWith, Error, Result};

/// What a test runs inside a commit, handed the store the commit is writing.
#[cfg(test)]
type Hook = Box<dyn FnOnce(&crate::Store)>;

#[cfg(test)]
thread_local! {
    /// Run once, on this thread, between a commit's placement and its write
    /// gate: the window a split must not fall into (ADR-0095 D8).
    static AFTER_PLACEMENT: std::cell::RefCell<Option<Hook>> =
        const { std::cell::RefCell::new(None) };
}

/// Where a commit landed: one position, in one log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    /// The log the position counts in.
    pub log: LogId,
    /// The position the commit was written at.
    pub sequence: Sequence,
}

/// What becomes of a settled transaction's batch.
#[derive(Debug, Clone, Copy)]
enum Settle {
    /// Write it. This is a commit.
    Apply,
    /// Drop it, every check having run. This is a rehearsal.
    Discard,
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        self.store
            .snapshot_registry()
            .release(self.snapshot, self.registered);
    }
}

/// Wait before re-racing for the committed tail, having lost `attempt` times.
///
/// The wait doubles with each loss and is then taken **uniformly at random from
/// zero up to that bound** rather than used as it stands. Full jitter, and the
/// randomness is the load-bearing part: writers that lose together and then wait
/// the same amount arrive together, which reproduces the collision the wait was
/// meant to break. Spreading them across a widening window is what makes the
/// second attempt likely to succeed instead of merely later.
///
/// Nothing is held across this wait: the writer gives its turn at the write gate
/// back before it sleeps (`crate::gate`), so a waiting writer blocks only itself.
fn back_off(attempt: u32) {
    std::thread::sleep(waiting_for(attempt, jitter()));
}

/// How long to wait, given the attempt and a number that differs per thread.
///
/// Separated from the sleep so that the arithmetic — the doubling, the ceiling,
/// the reduction of the jitter into the window — can be asserted without a test
/// that spends the wait it is checking. What is left in [`back_off`] is one
/// call and one sleep.
fn waiting_for(attempt: u32, jitter: u64) -> Duration {
    let window = window_for(attempt);
    Duration::from_micros(jitter.checked_rem(window.max(1)).unwrap_or(0))
}

/// The widest this attempt may wait, in microseconds.
///
/// Doubling per loss up to the ceiling. Separate from [`waiting_for`] so that
/// the window can be asserted as itself: reducing a jitter into it is a
/// different property, and a test that tried to recover the window from a wait
/// would be asserting a modulo rather than a bound.
fn window_for(attempt: u32) -> u64 {
    // `attempt` is capped before the shift because shifting a `u64` by 64 or
    // more panics in debug and wraps in release — the pair of behaviours this
    // workspace refuses to leave to chance. The cap sits far above any attempt
    // the budget allows, so it never fires in practice and is not a knob.
    let doubling = COMMIT_BACKOFF_STEP.saturating_mul(1_u64 << attempt.min(16));
    doubling.min(COMMIT_BACKOFF_CEILING)
}

/// A number that differs between the threads racing to commit.
///
/// A per-thread xorshift, seeded once from the standard library's own hasher
/// keys — which are randomised per process — mixed with the address of the
/// thread-local itself so that two threads in one process start apart. It is not
/// cryptographic and does not need to be: nothing here is a secret, and the only
/// property required is that two writers do not compute the same wait.
///
/// A dependency-free source on purpose. The store's other randomness reads
/// `/dev/urandom`, which is right for a node identity written once and far too
/// heavy for something consulted on a contended write path.
fn jitter() -> u64 {
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|state| {
        let mut held = state.get();
        if held == 0 {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_usize(std::ptr::from_ref(state).addr());
            // Zero is the "not yet seeded" mark and is also the one value
            // xorshift cannot leave, so it is replaced rather than accepted.
            held = hasher.finish() | 1;
        }
        held ^= held << 13;
        held ^= held >> 7;
        held ^= held << 17;
        state.set(held);
        held
    })
}

/// Is this address the leadership table?
///
/// A free function and not a method, because it is a fact about an address and
/// nothing about the transaction holding it — and because the gate above reads
/// better when the condition it turns on has a name.
fn is_a_leadership(address: &RecordAddress) -> bool {
    address.namespace == crate::catalog::system::SYSTEM_NAMESPACE
        && address.database == crate::catalog::system::SYSTEM_DATABASE
        && address.table == crate::catalog::system::LEADERSHIPS
}

/// Which shard each record a commit writes falls in (G031, ADR-0080).
#[derive(PartialEq, Eq)]
pub(crate) struct Placement {
    maps: BTreeMap<TableId, Arc<ShardMap>>,
}

impl Placement {
    /// The shard `address` falls in, or `None` when its table is not split.
    pub(crate) fn shard_of(&self, address: &RecordAddress) -> Option<ShardId> {
        self.maps
            .get(&address.table)
            .map(|map| map.shard_of(&address.id))
    }
}

impl Transaction<'_> {
    /// Buffer a write. Nothing reaches the store until commit.
    pub fn put(&mut self, address: RecordAddress, payload: Vec<u8>) {
        // A plain write replaces an expiring one whole, instant included — the
        // rule a key-value `SET` keeps in the system this engine's cache
        // semantics follow, and the only reading under which a write says
        // everything about the version it makes.
        if let Some(at) = self.expiring.remove(&address) {
            self.lifetimes
                .insert(address.clone(), super::Lifetime::Carried(at));
        }
        self.writes.insert(address, RecordValue::Present(payload));
    }

    /// Make the write already buffered for `address` never expire, on purpose
    /// (`EXPIRE NONE`, `PERSIST`) — which a table that declares expiry tells
    /// apart from a write that merely said nothing (ADR-0122 A3).
    pub fn persist_pending(&mut self, address: &RecordAddress) {
        self.expiring.remove(address);
        self.lifetimes
            .insert(address.clone(), super::Lifetime::Cleared);
    }

    /// Make the write already buffered for `address` stop being answered at
    /// `at`, in milliseconds since the Unix epoch (G035).
    ///
    /// Applied **after** the write, so the value takes the same road every write
    /// takes — schema, sealing, identity — and the instant is the only thing
    /// added. Nothing happens when no present value is buffered there: an
    /// instant on a deletion is meaningless.
    ///
    /// An instant at or before this transaction's clock turns the write into a
    /// **deletion**: the version would be gone the moment it was written, so
    /// writing it would only leave a value no reader can reach for the removal
    /// pass to find. It also means every read of this transaction's own writes
    /// is right without knowing expiry exists, because the transaction judges
    /// with one clock for its whole life.
    pub fn expire_pending(&mut self, address: &RecordAddress, at: u64) {
        if !matches!(self.writes.get(address), Some(RecordValue::Present(_))) {
            return;
        }
        if at <= self.reading_at() {
            self.delete(address.clone());
            return;
        }
        self.lifetimes.remove(address);
        self.expiring.insert(address.clone(), at);
    }

    /// The millisecond this transaction judges expiry and retention against.
    ///
    /// Public so that a writer computes "thirty seconds from now" on the same
    /// clock the reader of its own write will use.
    #[must_use]
    pub fn clock(&self) -> u64 {
        self.reading_at()
    }

    /// The instant a record stops being answered at, as this transaction sees
    /// it: its own buffered write first, then the stored version.
    ///
    /// `None` both for a record that never expires and for one that is not
    /// there — ask [`Transaction::get`] to tell the two apart.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn expires(&self, address: &RecordAddress) -> Result<Option<u64>> {
        if self.writes.contains_key(address) {
            return Ok(self.expiring.get(address).copied());
        }
        let now = self.reading_at();
        Ok(self
            .read_stamped_at(address)?
            .and_then(|stamped| stamped.expires().filter(|at| *at > now)))
    }

    /// Buffer a delete.
    ///
    /// A delete is a version carrying a tombstone, not an erased key: a reader
    /// at an older snapshot must still see the record.
    pub fn delete(&mut self, address: RecordAddress) {
        self.expiring.remove(&address);
        self.lifetimes.remove(&address);
        self.writes.insert(address, RecordValue::Tombstone);
    }

    /// Whether this transaction has written anything a commit would land.
    ///
    /// A read commits too — an empty transaction — and nothing about it is
    /// worth waiting for copies of (ADR-0106).
    #[must_use]
    pub fn writes_anything(&self) -> bool {
        !self.writes.is_empty()
    }

    /// The range this transaction's commit lands in — the home whose log
    /// [`commit_placed`](Self::commit_placed) will name (ADR-0106).
    ///
    /// The commit's own derivation, run early, so a write that must wait for
    /// copies is judged against where it actually lands: a membership row is a
    /// store-wide write whatever namespace the session has selected.
    ///
    /// # Errors
    ///
    /// A substrate failure reading what the records were before.
    pub fn home(&self) -> Result<Reach> {
        let identity = self.store.node_identity()?;
        let placement = self.placement()?;
        let record = self.log_record(identity.id, &placement)?;
        crate::catalog::home_of(&record)
    }

    /// Discard the transaction.
    ///
    /// Nothing was written, so nothing is undone. Dropping the transaction does
    /// the same thing; this exists to say so at the call site.
    pub fn rollback(self) {
        drop(self);
    }

    /// Commit every buffered write at one new sequence.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] when another transaction committed to a
    /// record this one wrote, [`Error::CommitContention`] when every attempt
    /// lost the race for the committed tail, or a substrate error.
    pub fn commit(self) -> Result<Sequence> {
        self.settle(Settle::Apply)
            .map(|committed| committed.sequence)
    }

    /// [`commit`](Self::commit), answering the log the position counts in.
    ///
    /// A commit lands in ONE log — the line's log of the home it writes under a
    /// leadership, this node's own otherwise — and a write waiting for a
    /// majority waits for followers to hold that log through that position
    /// (ADR-0106 D6). A position without its log is a number in no counter.
    ///
    /// # Errors
    ///
    /// The same as [`commit`](Self::commit).
    pub fn commit_placed(self) -> Result<Committed> {
        self.settle(Settle::Apply)
    }

    /// Run every check a commit runs, then discard the work.
    ///
    /// This is how a caller finds out whether a write would be refused without
    /// writing it. `rollback` cannot answer that question: it discards without
    /// checking, and **every** check that refuses a write runs inside the
    /// commit — so a transaction that is cancelled is a transaction nothing ever
    /// disagreed with.
    ///
    /// # Why it is this function and not a validation pass
    ///
    /// It is the commit with one line skipped, and it is written that way on
    /// purpose. A second path that checks the same things would agree with this
    /// one until it did not, and a rehearsal that disagrees with the performance
    /// is worse than no rehearsal, because somebody trusted it. So the conflict
    /// check, the schema validation and index maintenance are the same calls in
    /// the same order — index maintenance especially, since a unique violation
    /// is raised there and is exactly the refusal worth rehearsing.
    ///
    /// # Errors
    ///
    /// Every failure [`commit`](Self::commit) can return except those the write
    /// itself would raise: nothing is applied, so the substrate is not asked to.
    pub fn dry_run(self) -> Result<()> {
        self.settle(Settle::Discard).map(|_| ())
    }

    /// Everything this transaction changed, as the log will carry it.
    ///
    /// Built once, before the retry loop: the mutations do not depend on which
    /// sequence the commit eventually wins, so rebuilding them per attempt would
    /// be work that also invites the two attempts to differ.
    ///
    /// # The stamp is produced here, and here is the only place it can be
    ///
    /// Each version carries what its writer had **seen**: the stamp standing on
    /// the version this write replaces, with this node's own count raised by one
    /// and every other node's carried across unchanged. That carrying is the
    /// mechanism — it is what lets a later comparison tell a write that saw
    /// another from a write that was made in ignorance of it, which is the only
    /// distinction a multi-master range has to work from (G027 S2.2, Q-636).
    ///
    /// `node` is the identity the caller already read for the leadership checks
    /// rather than one fetched again, and it is the same value the log's own
    /// name carries as its [`tessari_encoding::Writer`] — one node axis, used by
    /// the key and by the stamp, so the two cannot come to disagree about which
    /// node wrote a record.
    ///
    /// **What it deliberately does not do.** The stamp is advanced from the
    /// **newest** stored version and not from the merge of every surviving one.
    /// A store holding two concurrent versions at once needs the merge — and it
    /// cannot hold two until the engine decides what a write meeting a
    /// concurrency does, which is S3's question and not this criterion's
    /// (Q-645).
    pub(super) fn log_record(
        &self,
        node: [u8; tessari_encoding::NODE_ID_LEN],
        placement: &Placement,
    ) -> Result<LogRecord> {
        let mut mutations = Vec::with_capacity(self.writes.len());
        for (address, value) in &self.writes {
            let mut stamp = self
                .read_newest_stamped(address)?
                .map_or_else(CausalStamp::new, |(_, stamped)| stamped.stamp().clone());
            stamp.advance(node);
            mutations.push(Mutation {
                namespace: address.namespace,
                database: address.database,
                table: address.table,
                id: address.id.clone(),
                shard: placement.shard_of(address),
                value: match self.expiring.get(address) {
                    Some(at) => StampedValue::stamped(stamp, value.clone()).expiring(*at),
                    None => StampedValue::stamped(stamp, value.clone()),
                },
            });
        }
        Ok(self.mark_across(LogRecord::new(mutations)))
    }
}

#[cfg(test)]
mod tests;
