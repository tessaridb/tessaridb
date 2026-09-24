//! Writing, committing, and losing the race.
//!
//! A commit is the only place this type touches the store, and the only place a
//! conflict can be raised. The backoff below is what a loser does before trying
//! again: randomised, so two threads that lost the same race do not re-enter it
//! together.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tessari_constants::{COMMIT_BACKOFF_CEILING, COMMIT_BACKOFF_STEP, MAX_COMMIT_ATTEMPTS};
use tessari_encoding::{
    CausalStamp, LogId, LogRecord, Mutation, RecordValue, StampedValue, decode_payload,
};
use tessari_types::{Sequence, ShardId, TableId};

use super::{RecordAddress, Transaction};
use crate::catalog::{Reach, ShardMap};
use crate::error::{Error, Result};

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
        self.store.snapshot_registry().release(self.snapshot);
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
/// Nothing is held across this wait. The commit takes no lock — it races on the
/// substrate's conditional apply — so a waiting writer blocks only itself.
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
            hasher.write_usize(std::ptr::from_ref(state) as usize);
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
struct Placement {
    maps: BTreeMap<TableId, Arc<ShardMap>>,
}

impl Placement {
    /// The shard `address` falls in, or `None` when its table is not split.
    fn shard_of(&self, address: &RecordAddress) -> Option<ShardId> {
        self.maps
            .get(&address.table)
            .map(|map| map.shard_of(&address.id))
    }
}

impl Transaction<'_> {
    /// Buffer a write. Nothing reaches the store until commit.
    pub fn put(&mut self, address: RecordAddress, payload: Vec<u8>) {
        self.writes.insert(address, RecordValue::Present(payload));
    }

    /// Buffer a delete.
    ///
    /// A delete is a version carrying a tombstone, not an erased key: a reader
    /// at an older snapshot must still see the record.
    pub fn delete(&mut self, address: RecordAddress) {
        self.writes.insert(address, RecordValue::Tombstone);
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

    /// The ranges this transaction's writes address, deduplicated.
    ///
    /// A [`Reach::Database`] per write, because that is the narrowest range a
    /// record belongs to and [`Reach::contains`] widens it: a leadership over
    /// the namespace or over the whole store covers these without the gate
    /// having to construct those ranges itself.
    ///
    /// # A leadership row is judged by the range it describes
    ///
    /// One exception, and it is the wall Q-597 named before anything could reach
    /// it. A leadership row lives in the system tenancy, so by address it is a
    /// write into `Reach::Database(0, 0)` — and in a cluster where somebody else
    /// holds `Reach::Store`, that range is led elsewhere. A node that has just
    /// won a round for `Namespace(Y)` would therefore be refused permission to
    /// record the leadership a majority granted it: the lease is installed and
    /// cannot fail, but the log never learns about it, so every other node goes
    /// on routing `Namespace(Y)`'s writes to the store-wide leader.
    ///
    /// The row is a **claim about `Namespace(Y)`**, so the question worth asking
    /// is who leads `Namespace(Y)` — which is this node, by construction, because
    /// the row exists only because it won that round. Asking instead who leads
    /// the tenancy the row happens to be stored in is asking about the filing
    /// cabinet rather than the document.
    ///
    /// This is narrower than exempting the system tenancy, which was the other
    /// way out and would have let any node holding any lease write any system
    /// row — another node's membership included.
    ///
    /// A row that cannot be decoded is **refused** rather than judged by its
    /// address: a leadership whose range this node cannot read is one it cannot
    /// place, and placing it wrongly is the failure this whole function exists to
    /// prevent.
    /// Whether **every** range this transaction writes was declared multi-master.
    ///
    /// All of them and not any of them. A transaction that writes a declared
    /// range and an undeclared one is still a write into a range that has a
    /// single leader, and exempting it because one of its ranges was declared
    /// would let the undeclared write travel under the declared one's cover.
    fn every_range_admits_two_writers(&self, ranges: &BTreeSet<Reach>) -> Result<bool> {
        for range in ranges {
            if !self.store.admits_two_writers(*range)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Refuse a placed range this node may not write on its own line, and
    /// answer the ranges left to the store line (ADR-0082).
    ///
    /// A live line admits; a spent one is `LeaseSpent` for that range alone; no
    /// line at all is `NoLeadershipYet` in a cluster — unless the range admits
    /// two writers, by the same predicate the store line's question consults.
    /// The store leader is deliberately NOT a fallback for a placed range with
    /// no live leader: the placement carved it out, and writing it anyway is
    /// the two-writer window the carving exists to close.
    fn admitted_on_their_lines(
        &self,
        ranges: &BTreeSet<Reach>,
        placed: &BTreeSet<Reach>,
        me: &[u8; tessari_encoding::NODE_ID_LEN],
    ) -> Result<BTreeSet<Reach>> {
        let mut on_the_store = BTreeSet::new();
        for range in ranges {
            let line = crate::catalog::governing(placed, *range);
            if line == Reach::Store {
                on_the_store.insert(*range);
                continue;
            }
            match self.store.line_standing(line) {
                crate::lines::Standing::Live => {}
                crate::lines::Standing::Spent(for_the_last) => {
                    return Err(Error::LeaseSpent { for_the_last });
                }
                crate::lines::Standing::NotHeld => {
                    if self.store.in_a_cluster(me)? && !self.store.admits_two_writers(*range)? {
                        return Err(Error::NoLeadershipYet);
                    }
                }
            }
        }
        Ok(on_the_store)
    }

    fn ranges_written(&self, placement: &Placement) -> Result<BTreeSet<Reach>> {
        self.writes
            .iter()
            .map(|(address, value)| match value {
                RecordValue::Present(payload) if is_a_leadership(address) => {
                    let described = crate::catalog::LeadershipDefinition::from_value(
                        &decode_payload(payload)?,
                    )?;
                    Ok(described.range)
                }
                _ => Ok(match placement.shard_of(address) {
                    Some(shard) => {
                        Reach::Shard(address.namespace, address.database, address.table, shard)
                    }
                    None => Reach::Database(address.namespace, address.database),
                }),
            })
            .collect()
    }

    /// The shard maps of every split table this transaction writes (G031).
    ///
    /// Resolved once and asked twice — by the admission gate, for the ranges it
    /// judges, and by the log record, for the shard it stamps on each mutation —
    /// because two lookups of one fact are two answers that can disagree, and a
    /// record admitted under one shard and filed under another is exactly the
    /// failure the stamp exists to make impossible.
    ///
    /// The registry answers almost always; a miss reads the committed catalog
    /// once and teaches the registry, which is what a node that became leader
    /// after the tables were declared elsewhere meets on its first write.
    fn placement(&self) -> Result<Placement> {
        let mut maps: BTreeMap<TableId, Arc<ShardMap>> = BTreeMap::new();
        let mut unread: BTreeSet<TableId> = BTreeSet::new();
        for address in self.writes.keys() {
            if address.namespace == crate::catalog::system::SYSTEM_NAMESPACE
                || maps.contains_key(&address.table)
                || unread.contains(&address.table)
            {
                continue;
            }
            match self.store.shards().known(address.table) {
                Some(Some(map)) => {
                    maps.insert(address.table, map);
                }
                Some(None) => {}
                None => {
                    unread.insert(address.table);
                }
            }
        }
        if !unread.is_empty() {
            let mut view = self.store.begin()?;
            let catalog = crate::catalog::Catalog::new(&mut view);
            for table in unread {
                // A record naming a table the catalog does not hold is not split
                // by anything this store knows of, and is filed as it always was;
                // whether such a write is allowed at all is not this function's
                // question. It is NOT learned, so a table declared later is read
                // again rather than remembered as unsplit.
                let Some(definition) = catalog.table(table)? else {
                    continue;
                };
                self.store.shards().learn(table, definition.shards.as_ref());
                if let Some(map) = definition.shards {
                    maps.insert(table, Arc::new(map));
                }
            }
        }
        Ok(Placement { maps })
    }

    fn settle(self, settle: Settle) -> Result<Sequence> {
        if self.writes.is_empty() {
            // The log position, not this transaction's snapshot. Nothing was
            // committed, so neither answer is a position anything was written
            // at — but the return names a log position, and the snapshot stopped
            // being one when the version was separated from it (Q-614).
            return self.store.committed_tail(
                self.store
                    .own_log(crate::store::UNPARTITIONED_REPORT_HOME)?,
            );
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
        if !self.store.holds_lines() {
            if let Some(for_the_last) = self.store.lease_spent() {
                return Err(Error::LeaseSpent { for_the_last });
            }
        }
        // And before the store-wide question, because the store-wide question
        // returns early on a live lease and would therefore never reach a leader
        // — while a leader writing into a range somebody else leads is exactly
        // what this catches (G025 S6.1). *May this node write* and *may this
        // node write HERE* stopped being one question the moment two nodes could
        // lead two namespaces.
        let identity = self.store.node_identity()?;
        // Resolved once and asked twice: both admission questions are about the
        // ranges this transaction writes, and deriving them separately is how
        // two questions about one thing come to disagree about what that thing
        // was.
        let placement = self.placement()?;
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
        let record = self.log_record(identity.id, &placement)?;
        // The log this commit belongs to, derived from the record before the
        // loop because it cannot change between attempts: it is a property of
        // what is being written, not of the state being written onto. The
        // position is allocated from this home's counter, which is the whole of
        // what "the sequence is per-range" means at the write end.
        // And the writer, which is THIS node: a commit allocates into its own
        // log and never into another writer's. That is the whole of what S2.2
        // means at the write end — two masters on one range are two counters,
        // and a node that allocated from the other's would be back to one.
        let log = LogId::new(crate::catalog::home_of(&record)?, self.store.writer()?);

        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            if attempt > MAX_COMMIT_ATTEMPTS {
                log::error!("commit gave up after {MAX_COMMIT_ATTEMPTS} attempts");
                return Err(Error::CommitContention {
                    attempts: MAX_COMMIT_ATTEMPTS,
                });
            }

            let tail = self.store.committed_tail(log)?;
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
            // Index entries are derived here rather than carried in the record,
            // and they are derived inside the loop because they depend on the
            // committed state this attempt is building on (see `crate::index`).
            let batch = crate::index::maintain(
                self.store,
                &record,
                crate::log::apply_batch(log, commit_at, commit_version, &record),
            )?;
            // Adjacency is derived in the same place and for the same reason: a
            // replica reaches its state by replaying this record, so entries the
            // leader merely added to its own batch would never exist on a
            // follower — a walk that finds nothing there while the leader is
            // correct, with nothing in an error state.
            let batch = crate::adjacency::maintain(self.store, &record, batch)?;
            // And the record counts, in the same batch and for the third time
            // for the same reason: the planner on a follower must read the same
            // number as the planner on the leader, or one query takes two access
            // paths depending on which node answered it.
            let batch = crate::cardinality::maintain(self.store, &record, batch, commit_version)?;
            // Everything above this ran. This is the whole difference between a
            // rehearsal and a write, and it is one line so that it can only ever
            // be the whole difference.
            if matches!(settle, Settle::Discard) {
                return Ok(commit_at);
            }

            match self.store.backend().apply(batch) {
                Ok(()) => {
                    // Counted HERE and not where it was decided. The decision is
                    // re-taken on every attempt, so an attempt that loses its
                    // batch would otherwise count a loss it never caused — and
                    // the `Settle::Discard` return above this line skips it for
                    // the same reason, because a rehearsal discards nothing.
                    if discarded > 0 {
                        self.store.discarded(discarded);
                    }
                    return Ok(commit_at);
                }
                // The position moved between reading it and applying, so the
                // conflict check above was made against a stale state and the
                // whole attempt is repeated rather than patched up — after
                // waiting, so that this attempt does not re-race into the same
                // instant as every other loser.
                Err(tessari_kv::Error::Conflict { .. }) => {
                    // At debug: one contended key under load produces this line
                    // per loser per attempt, and a retry that then succeeds is
                    // the design working rather than an event.
                    log::debug!("commit lost attempt {attempt}, retrying");
                    back_off(attempt);
                    continue;
                }
                Err(other) => return Err(other.into()),
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
    fn log_record(
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
                value: StampedValue::stamped(stamp, value.clone()),
            });
        }
        Ok(LogRecord::new(mutations))
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
    /// One scan of that record's versions per written address, bounded by the
    /// versions above the reclaim floor. Asked of the addresses this transaction
    /// writes and of nothing else. The table's declaration is read only after
    /// two survivors have been found, so a settled record never reaches the
    /// catalog for it.
    ///
    /// # Errors
    ///
    /// [`Error::ConcurrentVersions`] when a contested record's table has not
    /// declared what to do, and the backend's failure when the versions or the
    /// declaration cannot be read.
    fn refuse_a_contested_record(&self) -> Result<u64> {
        let mut discarded = 0_u64;
        for address in self.writes.keys() {
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
    fn check_for_conflicts(&self) -> Result<()> {
        for address in self.writes.keys() {
            let Some((version, _)) = self.read_newest(address)? else {
                continue;
            };
            if version > self.snapshot {
                return Err(Error::Conflict {
                    id: address.id.clone(),
                    snapshot: self.snapshot,
                    committed: version,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMMIT_BACKOFF_CEILING, COMMIT_BACKOFF_STEP, MAX_COMMIT_ATTEMPTS, jitter, waiting_for,
        window_for,
    };

    #[test]
    fn the_wait_doubles_until_it_reaches_the_ceiling_and_then_stops() {
        // The property the doubling exists for: the window a loser is spread
        // across widens with the contention rather than being guessed in
        // advance. Asserted on the *bound*, by handing in a jitter that always
        // lands at the top of the window, because the wait itself is random by
        // design and a test that asserted an exact wait would be asserting the
        // jitter away.
        let mut previous = 0;
        let mut flattened = false;
        for attempt in 1..=MAX_COMMIT_ATTEMPTS {
            let now = window_for(attempt);
            assert!(
                now >= previous,
                "attempt {attempt} has a narrower window than the one before it: {now} < {previous}"
            );
            assert!(
                now <= COMMIT_BACKOFF_CEILING,
                "attempt {attempt} opens a window of {now}, past the ceiling"
            );
            if now == previous && attempt > 1 {
                flattened = true;
            }
            previous = now;
        }
        assert_eq!(
            window_for(MAX_COMMIT_ATTEMPTS.saturating_add(4)),
            COMMIT_BACKOFF_CEILING,
            "the doubling never reaches the ceiling, so it is not bounded by it"
        );
        // Not a fact about the constants so much as a check that they still say
        // what the doc comment claims: the budget is spent before the ceiling
        // makes the last attempts indistinguishable. If a future value of
        // MAX_COMMIT_ATTEMPTS flattens the tail, this says so.
        assert!(
            !flattened,
            "the window flattens before the budget is spent, so the last attempts no longer widen"
        );
    }

    #[test]
    fn the_wait_stays_inside_its_window_whatever_the_jitter_is() {
        for attempt in 0..64_u32 {
            for offered in [0, 1, 7, 4_999, u64::MAX / 3, u64::MAX] {
                let waited = waiting_for(attempt, offered).as_micros();
                assert!(
                    waited < u128::from(window_for(attempt).max(1)),
                    "attempt {attempt} with jitter {offered} waited {waited}, \
                     outside its own window of {}",
                    window_for(attempt)
                );
            }
        }
    }

    #[test]
    fn a_first_loss_still_waits_rather_than_re_racing_into_the_same_instant() {
        // The whole point, reduced to one row. A jitter that reduces to zero is
        // allowed — full jitter includes zero — so the assertion is on the
        // window rather than on every draw: attempt one must have room to wait.
        assert!(
            window_for(1) >= COMMIT_BACKOFF_STEP,
            "the first retry has no window to spread into"
        );
    }

    #[test]
    fn two_threads_do_not_compute_the_same_wait() {
        // The jitter is load-bearing rather than decorative: writers that lose
        // together and wait the same amount arrive together, which rebuilds the
        // collision the wait exists to break. Drawn many times per thread
        // because a single pair could coincide by chance, and compared as
        // sequences because two threads sharing a seed would agree on all of it.
        let mine: Vec<u64> = (0..32).map(|_| jitter()).collect();
        let theirs = std::thread::spawn(|| (0..32).map(|_| jitter()).collect::<Vec<u64>>())
            .join()
            .expect("the other thread drew its own");
        assert_ne!(mine, theirs, "two threads drew the same sequence of waits");
    }

    #[test]
    fn one_thread_does_not_draw_one_number_forever() {
        // Guards the seeding: a state left at zero would xorshift to zero for
        // ever, and every retry would wait exactly nothing — which is the
        // behaviour being removed, arriving back through the jitter.
        let drawn: std::collections::BTreeSet<u64> = (0..32).map(|_| jitter()).collect();
        assert!(drawn.len() > 1, "the jitter is a constant: {drawn:?}");
        assert!(
            !drawn.contains(&0),
            "the jitter state reached zero and stuck"
        );
    }
}
