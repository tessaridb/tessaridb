//! Writing, committing, and losing the race.
//!
//! A commit is the only place this type touches the store, and the only place a
//! conflict can be raised. The backoff below is what a loser does before trying
//! again: randomised, so two threads that lost the same race do not re-enter it
//! together.

#[cfg(test)]
mod moved_map;
mod ranges;
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
    pub(super) fn shard_of(&self, address: &RecordAddress) -> Option<ShardId> {
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
        self.expiring.remove(&address);
        self.writes.insert(address, RecordValue::Present(payload));
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

    fn settle(mut self, settle: Settle) -> Result<Committed> {
        // A decision across leaders writes no records and is still a commit.
        if self.writes.is_empty() && !self.is_across() {
            // The log position, not this transaction's snapshot. Nothing was
            // committed, so neither answer is a position anything was written
            // at — but the return names a log position, and the snapshot stopped
            // being one when the version was separated from it (Q-614).
            let log = self
                .store
                .own_log(crate::store::UNPARTITIONED_REPORT_HOME)?;
            return Ok(Committed {
                log,
                sequence: self.store.committed_tail(log)?,
            });
        }
        // First, and after the empty check rather than before it. First because
        // a node that has run out of leadership should not be doing schema
        // validation on work it is about to refuse; after the empty check
        // because a transaction that writes nothing has nothing to fence, and
        // refusing it would make a fenced node fail its readers' commits.
        //
        // Here rather than at the statement layer so that `dry_run` rehearses
        // it — this function's own header is the argument, and a fence a
        // `VERIFY` cannot see is a refusal an operator meets for the first time
        // in production.
        //
        // Only while this node holds no placed range's line (ADR-0082): once it
        // does, a spent store lease refuses the ranges the store line governs
        // and not the ones a live line of their own does, so the question waits
        // below until the ranges are known.
        let held_lines = self.store.holds_lines();
        if !held_lines && let Some(for_the_last) = self.store.lease_spent() {
            return Err(Error::LeaseSpent { for_the_last });
        }
        // And before the store-wide question, because the store-wide question
        // returns early on a live lease and would therefore never reach a leader
        // — while a leader writing into a range somebody else leads is exactly
        // what this catches (G025 S6.1). *May this node write* and *may this
        // node write HERE* stopped being one question the moment two nodes could
        // lead two namespaces.
        let identity = self.store.node_identity()?;
        // Counted across both loops: a restart for a moved map spends the same
        // budget a lost race does, so a map that keeps moving cannot hold a
        // commit for ever.
        let mut attempt = 0_u32;
        'placed: loop {
            // Resolved once and asked twice: both admission questions are about the
            // ranges this transaction writes, and deriving them separately is how
            // two questions about one thing come to disagree about what that thing
            // was.
            let placement = self.placement()?;
            #[cfg(test)]
            if let Some(hook) = AFTER_PLACEMENT.with(|held| held.borrow_mut().take()) {
                hook(self.store);
            }
            let ranges = self.ranges_written(&placement)?;
            let placed = self.store.refuse_if_led_elsewhere(&ranges, &identity.id)?;
            // ADR-0082: each written range is judged on the line that governs it. A
            // placed range is admitted only under a live lease on its own line, and
            // what is left is the store line's, judged exactly as it always was. A
            // store with no placement puts every range on the store line.
            let on_the_store = self.admitted_on_their_lines(&ranges, &placed, &identity.id)?;
            // And the other half of *the effective role is the lease* (ADR-0064):
            // a node that takes part in deciding writes under a leadership and at
            // no other time. Asked here rather than only at the statement layer for
            // the reason the paragraph above gives — `dry_run` must rehearse it, and
            // a refusal a `VERIFY` cannot see is one an operator meets for the first
            // time in production.
            // G027 S2.3 — and the declaration is what exempts it, by the SAME
            // predicate the divergence fence and the redirect consult, with no new
            // setting anywhere. The order matters and the `&&` is load-bearing:
            // `awaiting` returns on an in-memory lease read, so a node that holds a
            // leadership never reaches the catalog lookup, and the exemption is paid
            // for only by a commit that was otherwise about to be refused.
            if !on_the_store.is_empty() {
                if let Some(for_the_last) = self.store.lease_spent() {
                    return Err(Error::LeaseSpent { for_the_last });
                }
                if self.store.awaiting(&identity.id)?
                    && !self.every_range_admits_two_writers(&on_the_store)?
                {
                    return Err(Error::NoLeadershipYet);
                }
            }
            let store_line = !held_lines || !on_the_store.is_empty();
            let mut record = self.log_record(identity.id, &placement)?;
            // The log this commit belongs to, derived from the record before the
            // loop because it cannot change between attempts: it is a property of
            // what is being written, not of the state being written onto. The
            // position is allocated from this home's counter, which is the whole of
            // what "the sequence is per-range" means at the write end.
            // And the writer. Under a leadership, the line's one log of a
            // single-leader range, which the next leader continues (ADR-0107) —
            // and THIS node's own where the range admits two writers, since two
            // masters are two counters (S2.2). Under none — a store standing
            // alone, a node's declarations before it joins — its own log. The
            // leadership is asked once here and stamped on every attempt, so the
            // log and the epoch the record names cannot disagree.
            let home = crate::catalog::home_of(&record)?;
            let epoch = self.store.epoch_under(&placed, home);
            let log = if epoch > Epoch::ZERO && !self.admits_two_writers_here(home)? {
                tessari_encoding::LogId::line(home)
            } else {
                self.store.own_log(home)?
            };

            loop {
                attempt = attempt.saturating_add(1);
                if attempt > MAX_COMMIT_ATTEMPTS {
                    tracing::error!(attempts = MAX_COMMIT_ATTEMPTS, "commit gave up");
                    return Err(Error::CommitContention {
                        attempts: MAX_COMMIT_ATTEMPTS,
                    });
                }

                // Held from reading the tail to applying the batch, so no other
                // writer in this process can move what this attempt builds on
                // (`crate::gate`). Dropped at the end of the attempt, the wait
                // before a retry included.
                let turn = self.store.write_gate().hold();
                // ADR-0095 D8: the map this attempt was admitted and stamped
                // under is asked for again under the gate, because the statement
                // that moves a map teaches the registry under this same gate. A
                // map that moved in between would file these records in a shard
                // nothing writes again, so the whole admission is redone with
                // the new one — compared by value, since a re-learn of the same
                // map is a new `Arc` and no reason to start again.
                if self.placement()? != placement {
                    drop(turn);
                    tracing::debug!(
                        attempt,
                        "a shard map moved before the commit; placing again"
                    );
                    continue 'placed;
                }
                self.refuse_if_fenced_since(store_line, &placed, &ranges)?;
                let tail = self.store.committed_tail(log)?;
                // A prepare's half of the conflict check that its own node
                // could not make (ADR-0112 D3a), under the same turn.
                self.written_since_seen(log)?;
                self.check_for_conflicts()?;
                // Beside the conflict check, inside the loop, and for the same
                // reason: both ask whether the committed state this attempt builds
                // on will take the write, and a state that moved between attempts
                // must be re-read rather than assumed.
                //
                // AFTER it rather than before, so a record that is both moved and
                // contested answers `Conflict` first. That is the retryable one, and
                // a caller that retries meets the concurrency on the next attempt —
                // which is the right order to learn them in, because a stale write
                // has nothing useful to say about a conflict it never saw.
                let discarded = self.refuse_a_contested_record()?;
                // Inside the loop with the conflict check, and for the same reason:
                // both are read against the committed state this attempt builds on,
                // and a schema that moved between attempts must be re-read rather
                // than assumed.
                crate::schema::validate(self.store, &record)?;
                // A limited space's bound, against the same committed state and for
                // the same reason: two commits adding different keys do not conflict,
                // so only a count read here, per attempt, is exact (G036). The
                // attempt writes the record with its evictions when there are any.
                let mut evicted = crate::bounded::enforce(self.store, &record, identity.id)?;
                // A topic admits its messages against the same committed state and at
                // this transaction's clock: never a rewrite, never a deletion before
                // its retention passed — judged as a reader would judge it — never a
                // message over its size, and each new one carrying its expiry (G037).
                if let Some(admitted) = crate::topic::admit(
                    self.store,
                    evicted.as_ref().unwrap_or(&record),
                    self.clock(),
                    identity.id,
                )? {
                    evicted = Some(admitted);
                }
                let carried = match evicted.as_mut() {
                    Some(carrying) => carrying,
                    None => &mut record,
                };

                // Deciding the sequence locally is the *only* thing a commit does
                // that a replica's apply does not. Everything after this line is the
                // shared path.
                let commit_at = Sequence::new(tail.get().saturating_add(1));
                // And the version, separately, because it is a different fact: the
                // position is what a replica resumes from and compares, the version
                // is where this store's own history puts these records. Read inside
                // the loop for the same reason the tail is — a lost attempt built on
                // a state that has since moved (Q-614).
                let commit_version =
                    Sequence::new(self.store.committed_version()?.get().saturating_add(1));
                // And the version is the writer's ORDER, written into the record:
                // this node files its commits in one log per home, and a follower
                // applying those logs needs to know where each commit stood among
                // all of them — which only the writer knows (ADR-0084, Q-796).
                carried.set_order(commit_version);
                // And the leadership it is committed under, on the line that
                // governs its home — the epoch a follower refuses a second
                // history by and an election compares (ADR-0059); zero for a
                // node nobody made a leader.
                carried.set_epoch(epoch);
                let written = crate::log::apply_batch(log, commit_at, commit_version, carried);
                // A record of a transaction across leaders is checked and settled
                // here as a follower's apply settles it, and an intent derives
                // nothing until its resolution does (ADR-0112).
                let written = crate::intents::settle(self.store, carried, written, commit_version)?;
                let batch = if crate::intents::derives_nothing(carried) {
                    written
                } else {
                    // Index entries are derived here rather than carried in the record,
                    // and they are derived inside the loop because they depend on the
                    // committed state this attempt is building on (see `crate::index`).
                    let batch = crate::index::maintain(self.store, carried, written)?;
                    // Adjacency is derived in the same place and for the same reason: a
                    // replica reaches its state by replaying this record, so entries the
                    // leader merely added to its own batch would never exist on a
                    // follower — a walk that finds nothing there while the leader is
                    // correct, with nothing in an error state.
                    let batch = crate::adjacency::maintain(self.store, carried, batch)?;
                    // And the record counts, in the same batch and for the third time
                    // for the same reason: the planner on a follower must read the same
                    // number as the planner on the leader, or one query takes two access
                    // paths depending on which node answered it.
                    let batch =
                        crate::cardinality::maintain(self.store, carried, batch, commit_version)?;
                    // The expiry index, last and in the same batch as the records it
                    // describes: an entry written anywhere else is an entry that can be
                    // left behind (G035).
                    let batch = crate::lapse::maintain(self.store, carried, batch)?;
                    // And a limited space's modified-order index, which the evictions
                    // above read on the next commit (G036).
                    let batch =
                        crate::bounded::maintain(self.store, carried, batch, commit_version)?;
                    // And a topic's positions, dense in commit order (G037).
                    crate::topic::maintain(self.store, carried, batch)?
                };
                // Everything above this ran. This is the whole difference between a
                // rehearsal and a write, and it is one line so that it can only ever
                // be the whole difference.
                if matches!(settle, Settle::Discard) {
                    return Ok(Committed {
                        log,
                        sequence: commit_at,
                    });
                }
                // Before the batch can be read: a reader at this version must not be
                // answered from name or table rows held from before it.
                // And, under the same turn, every map this commit moves is taught
                // to the registry before the turn is handed on, because the next
                // commit to hold it re-asks its placement against that registry
                // (ADR-0095 D8). Forgotten again below if the batch does not land.
                let mut taught = Vec::new();
                if crate::catalog::CatalogRows::changes(carried) {
                    self.store.catalog_rows().changed(commit_version);
                    taught = self
                        .store
                        .shards()
                        .teach(self.store.decoded_tables(), carried)?;
                }

                // Staged rather than applied when the backend shares a sync
                // between writes, and the turn handed on before the wait: the next
                // writer derives on this batch while it is on its way to the
                // device, and whoever finds no landing running lands every staged
                // batch in one write (`crate::gate`). A backend with no sync to
                // share is applied under the turn, as grouping would only cost it.
                let backend = self.store.backend().as_ref();
                let landed = if backend.groups_writes() {
                    let ticket = self.store.write_gate().stage(batch);
                    drop(turn);
                    self.store.write_gate().land(ticket, backend)
                } else {
                    let applied =
                        self.store
                            .write_gate()
                            .apply(batch, backend, crate::gate::Landing::Synced);
                    drop(turn);
                    applied
                };
                match landed {
                    Ok(()) => {
                        // Counted HERE and not where it was decided. The decision is
                        // re-taken on every attempt, so an attempt that loses its
                        // batch would otherwise count a loss it never caused — and
                        // the `Settle::Discard` return above this line skips it for
                        // the same reason, because a rehearsal discards nothing.
                        if discarded > 0 {
                            self.store.discarded(discarded);
                        }
                        return Ok(Committed {
                            log,
                            sequence: commit_at,
                        });
                    }
                    // The position moved between reading it and applying — or the
                    // batch was derived on a staged one that did not land — so the
                    // conflict check above was made against a stale state and the
                    // whole attempt is repeated rather than patched up — after
                    // waiting, so that this attempt does not re-race into the same
                    // instant as every other loser.
                    Err(tessari_kv::Error::Conflict { .. }) => {
                        self.store.shards().forget(&taught);
                        // At debug: one contended key under load produces this line
                        // per loser per attempt, and a retry that then succeeds is
                        // the design working rather than an event.
                        tracing::debug!(attempt, "commit lost an attempt; retrying");
                        back_off(attempt);
                        continue;
                    }
                    Err(other) => {
                        self.store.shards().forget(&taught);
                        return Err(other.into());
                    }
                }
            }
        }
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

    /// Refuse the commit if any written record's stored versions disagree, and
    /// answer how many writes a declared last-writer-wins discarded instead.
    ///
    /// G027 S3.1 and the rule the goal exists for: a write concurrent with the
    /// stored version is **refused and named, never silently ranked**
    /// (ADR-0075).
    ///
    /// # Unless the table said otherwise, and then it is counted
    ///
    /// G027 S3.2. A table that declares `LAST WRITER WINS` takes the write, and
    /// the survivors it does not descend are returned as a count — spent by the
    /// caller only once the batch has actually landed. A count is the one thing
    /// that makes the discard observable: the losing version stays on disk
    /// byte-intact and vanishes from every answer, so without it an operator
    /// auditing storage finds both versions and concludes nothing was lost.
    ///
    /// # Why the refusal is here and not on the apply path
    ///
    /// A replica applying a record concurrent with what it holds must **accept**
    /// it. Refusing there would stop two masters' logs from ever meeting, which
    /// is what S2.2 asserts they do; holding several surviving versions is the
    /// whole purpose [`tessari_encoding::CausalVersions`] was built for.
    ///
    /// # And why it is not the incoming write compared against the stored one
    ///
    /// [`Self::log_record`] derives a commit's stamp *from* the newest stored
    /// version, so a local write always **descends** what it replaces and can
    /// never be concurrent with it. The concurrency a store meets is one that
    /// **arrived**, and what is refused is the next write made on top of it —
    /// by a writer holding one of two surviving versions, which cannot supersede
    /// the other without having seen it.
    ///
    /// # What it costs a settled record
    ///
    /// Nothing, outside a namespace that admits two writers. A second surviving
    /// version has one producer — a record applied from another writer's stream
    /// where the namespace's class admits two writers (`Store::apply`) — and a
    /// namespace's class is set when it is defined and never altered, so a
    /// record anywhere else has exactly one survivor and its versions are not
    /// read. That matters because a record can hold many: the one a table's
    /// generated identities are counted in is rewritten by every insert, and
    /// reading all its versions made a run of inserts quadratic (G058, Q-912's
    /// measurement). Asked once per namespace this transaction writes.
    ///
    /// Inside one, one scan of the record's versions per written address — all
    /// it still holds, which on a store that reclaims is those above the
    /// reclaim floor. The table's declaration is read only after two survivors
    /// have been found, so a settled record never reaches the catalog for it.
    ///
    /// # Errors
    ///
    /// [`Error::ConcurrentVersions`] when a contested record's table has not
    /// declared what to do, and the backend's failure when the versions or the
    /// declaration cannot be read.
    fn refuse_a_contested_record(&self) -> Result<u64> {
        let mut discarded = 0_u64;
        let mut two_writers: BTreeMap<NamespaceId, bool> = BTreeMap::new();
        for address in self.writes.keys() {
            let admits = match two_writers.get(&address.namespace) {
                Some(admits) => *admits,
                None => {
                    let admits = self
                        .store
                        .admits_two_writers(Reach::Namespace(address.namespace))?;
                    two_writers.insert(address.namespace, admits);
                    admits
                }
            };
            if !admits {
                continue;
            }
            let surviving = self.surviving_versions(address)?;
            let (Some((ours, our_stamp)), Some((theirs, their_stamp))) =
                (surviving.first(), surviving.get(1))
            else {
                continue;
            };
            // G027 S3.2 — unless the table said what to do, in which case this
            // is not a refusal at all. The lookup sits HERE, after two
            // survivors have been found, so it is paid for only by a commit
            // that was otherwise about to be refused: an ordinary write leaves
            // `surviving_versions` with one version and never reaches the
            // catalog. That is W291's placement, and the unedited
            // `counted_reads` admission test is what holds it.
            //
            // The last writer is this caller, not a timestamp. Nothing here
            // reads a clock: the incoming write supersedes every surviving
            // version including the ones it never saw, so the writes discarded
            // are the survivors it does not descend — every one but the newest,
            // which is the one the stamp producer stood on.
            if self
                .store
                .conflict_policy(address.table)?
                .discards_the_loser()
            {
                let lost = surviving.len().saturating_sub(1);
                discarded = discarded.saturating_add(u64::try_from(lost).unwrap_or(u64::MAX));
                continue;
            }
            // The node whose write `theirs` carries and `ours` does not. There
            // is one for every two-master case this engine can produce, and the
            // first in node order is named when there is more than one — the
            // stamp is held in node order, so "first" is a property of the
            // bytes rather than of the order they were read in.
            let unseen = their_stamp
                .entries()
                .iter()
                .find(|(node, seen)| *seen > our_stamp.count(node))
                .map(|(node, _)| *node)
                .unwrap_or_default();
            return Err(Error::ConcurrentVersions {
                id: address.id.clone(),
                ours: *ours,
                theirs: *theirs,
                node: unseen,
            });
        }
        Ok(discarded)
    }

    /// Refuse the commit if any written record has moved since the snapshot.
    ///
    /// This is the write-write detection, and it is only sound because the
    /// commit batch asserts the tail has not moved either — together they turn
    /// check-then-write into a compare-and-set over the whole commit.
    pub(super) fn check_for_conflicts(&self) -> Result<()> {
        // A guarded read is held to the same rule as a write: whatever decided
        // this transaction's writes must not have changed under it.
        let guarded = self.guarded.borrow();
        for address in self.writes.keys().chain(guarded.iter()) {
            // The newest version as stored, intents included — one read, as
            // before intents existed.
            let Some((version, provenance, _)) = self.newest_stored_value(address)? else {
                continue;
            };
            // A resolution writes over its own intents; anybody else's intent,
            // and this one's on a record it does not resolve, refuses.
            let intent = provenance
                .as_ref()
                .is_some_and(|provenance| provenance.provisional)
                && !self.resolves(provenance.as_ref());
            // ADR-0112 D6a: a version resolved from a transaction this one
            // does not see is one it read the version under instead of. Writing
            // over it would lose that transaction's write — an increment read
            // from the old value — so it is refused as a conflict, and passes
            // once this node's copies hold the transaction's every part.
            let unseen = match provenance.as_ref() {
                Some(resolved) if !resolved.provisional => !self.sees(resolved)?,
                _ => false,
            };
            // ADR-0112 D5: a standing intent refuses the write whatever this
            // writer's snapshot. An intent prepared before the snapshot is not
            // newer than it, and replacing the value under it would lose the
            // write the transaction across leaders is about to commit.
            // Retriable: the intent resolves.
            let with = match provenance {
                Some(held) if intent => ConflictWith::Intent(held.transaction),
                Some(held) if unseen => ConflictWith::Unseen(held.transaction),
                _ if version > self.snapshot => ConflictWith::Commit,
                _ => continue,
            };
            return Err(Error::Conflict {
                id: address.id.clone(),
                snapshot: self.snapshot,
                committed: version,
                with,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
