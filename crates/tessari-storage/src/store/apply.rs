//! Applying records that arrive from another node.

use std::sync::atomic::Ordering;

use tessari_encoding::{LogId, LogKey, LogRecord, StoreKey, Writer};
use tessari_types::{Epoch, Sequence};

use crate::error::{Error, Result};

use super::Store;

impl Store {
    /// Apply a record that arrived from a peer, which claims what stands
    /// before it.
    ///
    /// [`Self::apply_record`] refuses a divergence **only where the two logs
    /// overlap** — a record offered at a position this store already holds. It
    /// cannot see the case where they do not. A sender whose history parted
    /// from this store's at sequence 4 offers sequence 6; that is `tail + 1`
    /// here, so there is nothing at the position to compare and the record is
    /// appended. This store then holds 1-5 from one history and 6 from another,
    /// with no error anywhere and both nodes reporting healthy — which is the
    /// failure ADR-0059 exists to remove, one position further back than the
    /// record half reached.
    ///
    /// So the sender states the epoch of the record **before** the one it is
    /// offering, and this compares that against what it actually holds there.
    /// It is Raft's `AppendEntries` consistency check, which reads the
    /// follower's own entry at `prevLogIndex` and compares its term — not the
    /// follower's current term, and not the leader's.
    ///
    /// The refusal names **`at - 1`**: the position where the histories part,
    /// rather than the one where the check happened to run. An operator reading
    /// it needs the first number to know where to re-bootstrap from.
    ///
    /// The predecessor of sequence 1 is [`Epoch::ZERO`], which is what a store
    /// that has elected nobody holds — so the first record of a fresh log needs
    /// no special case at the call site.
    ///
    /// # Why this is a separate method and not a parameter
    ///
    /// A local commit knows its own predecessor by construction and has nothing
    /// to claim; a record arriving from a peer carries a claim about a history
    /// this store may not share. Those are different acts. An
    /// `Option<Epoch>` on [`Self::apply_record`] would make *the local path*
    /// and *a peer that said nothing* the same shape, and the check would then
    /// be skippable by forgetting a field.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogDivergence`] when the predecessor this store holds
    /// was written under a different leadership, and whatever
    /// [`Self::apply_record`] returns otherwise.
    pub fn apply_from_stream(
        &self,
        log: LogId,
        at: Sequence,
        previous: Epoch,
        record: &LogRecord,
    ) -> Result<()> {
        self.refuse_a_parted_history(log, at, previous)?;
        self.apply_record_in(log, at, record)
    }

    /// Apply one log record into a log the caller names.
    ///
    /// [`Self::apply_record`] derives the log from the record, which is right
    /// for every unfiltered path — a commit, a restore, a whole replay. A
    /// **selective** subscriber is given records with everything outside its
    /// reach removed, and a record emptied to nothing carries no mutation to
    /// derive a log from. Filing it at the store would put it in a counter it
    /// never came from and turn the next record of its real log into a gap.
    ///
    /// So this exists for exactly one caller, [`Self::apply_from_stream`], and
    /// takes the log the collect read. That is a fact about the collect rather
    /// than a second authority over the record: the leader answered from a log,
    /// and the follower files what it was given where it was read from.
    ///
    /// # Errors
    ///
    /// The same as [`Self::apply_record`].
    pub fn apply_record_in(&self, log: LogId, at: Sequence, record: &LogRecord) -> Result<()> {
        self.apply_at(log, at, record)
    }

    /// Apply one log record, at the sequence it carries.
    ///
    /// This is what a replica runs, and it is the same function a commit runs
    /// once it has decided its sequence locally.
    ///
    /// Re-applying a record the store already holds is a **no-op**, not an
    /// error: a replica that is re-sent a record it already has has not been
    /// told anything wrong, and refusing would turn an ordinary retry into an
    /// incident. Skipping *forward* is refused, because a gap means the state
    /// would no longer be explained by any log.
    ///
    /// # A retry and a divergence arrive the same way
    ///
    /// Both land on a position this store already holds, and until the log
    /// record carried an epoch there was nothing to tell them apart — so the
    /// second writer's record was discarded in silence and two nodes diverged
    /// while both reported healthy (ADR-0059). The comparison is against the
    /// epoch held **at that sequence**, read from the log, and not against the
    /// store's latest epoch: a follower catching up legitimately replays records
    /// from leaderships that have since ended, and every one of them would be a
    /// false divergence against the latest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogDivergence`] when another leadership wrote this position,
    /// [`Error::LogGap`] when the record is not the next one, and the mapped
    /// backend or decoding failure otherwise.
    pub fn apply_record(&self, writer: Writer, at: Sequence, record: &LogRecord) -> Result<()> {
        // Derived from the record rather than taken as a parameter, because it
        // is a property of the record and deriving it here is what makes a
        // replica file it where the leader filed it. A home the sender chose and
        // sent would be a second authority over the same fact.
        //
        // That argument holds only for a record that still has its mutations. A
        // SELECTIVE subscriber is given records with everything outside its
        // reach removed, and a record emptied to nothing has no mutation left to
        // derive a home from — it would file at the store, in a counter it never
        // came from, and the next record in its real log would then read as a
        // gap. So the filtered path names the log it collected from, which is a
        // fact about the collect and not a second authority over the record
        // (Q-620, Q-621). See `apply_record_in`.
        //
        // The WRITER is the other half and it is taken rather than derived,
        // because a record carries no writer to derive one from. That is not an
        // omission: the writer is named the way `apply_record_in` already names
        // the home for a selective subscriber — a fact about the COLLECT, not a
        // second authority over the record. The leader answered from a log; the
        // caller files what it was given where it was read from.
        self.apply_at(
            LogId::new(crate::catalog::home_of(record)?, writer),
            at,
            record,
        )
    }

    /// Apply one record into `home`, whatever named it.
    pub(super) fn apply_at(&self, log: LogId, at: Sequence, record: &LogRecord) -> Result<()> {
        // From the tail read to the apply, because the version this allocates is
        // the one a local commit allocates too (`crate::gate`).
        let _turn = self.writing.hold();
        // And with nothing staged behind a local commit's turn still to land:
        // this apply allocates the next version too, and writes its batch
        // itself rather than staging it.
        self.writing.land_all(self.backend.as_ref());
        let applied = self.committed_tail(log)?;
        if at.get() <= applied.get() {
            self.refuse_a_divergence(log, at, record.epoch())?;
            return Ok(());
        }
        let expected = Sequence::new(applied.get().saturating_add(1));
        if at != expected {
            return Err(Error::LogGap {
                expected,
                found: at,
            });
        }
        // A replica re-checks what the leader already checked. That is cheap
        // relative to the apply, and a violation reaching this point is a
        // divergence between two nodes' catalogs rather than a caller's mistake
        // — which is worth stopping at rather than writing through.
        crate::schema::validate(self, record)?;
        // Allocated here, locally, and deliberately not taken from `at`. A
        // replica numbers its own records: the log position it is replaying was
        // decided elsewhere, the version it writes them at is its own history
        // (Q-614). The two agree today because one flat log admits one writer.
        let version = Sequence::new(self.committed_version()?.get().saturating_add(1));
        let batch = crate::index::maintain(
            self,
            record,
            crate::log::apply_batch(log, at, version, record),
        )?;
        // Derived here as well as in the commit, because that is the whole
        // reason it is derived from the record: a follower that skipped this
        // would carry the edges and no way to walk them, and its walks would
        // answer nothing while the leader answered correctly — the symptom
        // `crate::adjacency`'s own header names as the reason it derives from
        // the mutation at all. It skipped it anyway, from W148 until W185,
        // because the replay called two of these three and nothing compared a
        // replica that held an edge (Q-452).
        //
        // The order matches the commit path deliberately: two paths that build
        // one batch in two orders are a difference waiting to become a
        // divergence nobody can explain.
        let batch = crate::adjacency::maintain(self, record, batch)?;
        // Derived here as well as in the commit, because that is the whole
        // reason it is derived from the record: a follower that skipped this
        // would carry the records and none of the counts, and its planner would
        // then choose a different access path for the same query.
        let batch = crate::cardinality::maintain(self, record, batch, version)?;
        // And the expiry index, so a follower promoted to leader can remove what
        // has expired without having written any of it (G035).
        let batch = crate::lapse::maintain(self, record, batch)?;
        let batch = crate::bounded::maintain(self, record, batch, version)?;
        let batch = crate::topic::maintain(self, record, batch)?;
        self.backend.apply(batch)?;
        Ok(())
    }

    /// Refuse a record from a leadership other than the one held at `at`.
    ///
    /// The vocabulary is the databases', not the chains': Kafka calls this log
    /// divergence and fixed it by putting a leader epoch in the log (KIP-101),
    /// PostgreSQL calls it a diverging timeline, MongoDB reaches the common
    /// point and rolls back. Only a leaderless design escapes it, by paying
    /// conflict resolution instead.
    ///
    /// Costs one point read and a fixed eight-byte inspection — the epoch sits
    /// in front of the mutations precisely so this does not decode the record —
    /// and it runs only on the branch a duplicate delivery takes.
    /// Refuse a record whose predecessor this store never wrote.
    ///
    /// The sibling of [`Self::refuse_a_divergence`], one position earlier. That
    /// one compares the record being offered against what stands at its own
    /// position; this one compares what the sender says stands **before** it
    /// against what actually does — which is the only way to catch a divergence
    /// that happened entirely behind this store's tail.
    ///
    /// Costs the same as its sibling: one point read and a fixed eight-byte
    /// inspection, no decode of the mutations.
    pub(super) fn refuse_a_parted_history(
        &self,
        log: LogId,
        at: Sequence,
        previous: Epoch,
    ) -> Result<()> {
        let Some(before) = at.get().checked_sub(1) else {
            return Ok(());
        };
        let before = Sequence::new(before);
        if before == Sequence::ZERO {
            // Nothing precedes the first record, and a store that has elected
            // nobody holds `Epoch::ZERO` — so a sender claiming anything else
            // is describing a history this store does not have.
            if previous == Epoch::ZERO {
                return Ok(());
            }
            self.divergences.fetch_add(1, Ordering::Relaxed);
            return Err(Error::LogDivergence {
                sequence: before,
                held: Epoch::ZERO,
                offered: previous,
            });
        }
        let stored = self
            .backend
            .get(LogKey::keyspace(), &LogKey::new(log, before).encode())?;
        // Nothing to compare against — this store is behind the sender by more
        // than one record, and `apply_record` refuses that with `LogGap`, which
        // is the more accurate answer. The truncation case is Q-529's, the same
        // hole the position check carries and the same decision: it belongs with
        // retention, because there is one answer for both.
        let Some(value) = stored else {
            return Ok(());
        };
        let held = LogRecord::epoch_in(value.as_slice())?;
        if held == previous {
            return Ok(());
        }
        self.divergences.fetch_add(1, Ordering::Relaxed);
        Err(Error::LogDivergence {
            sequence: before,
            held,
            offered: previous,
        })
    }

    pub(super) fn refuse_a_divergence(
        &self,
        log: LogId,
        at: Sequence,
        offered: Epoch,
    ) -> Result<()> {
        let stored = self
            .backend
            .get(LogKey::keyspace(), &LogKey::new(log, at).encode())?;
        // Nothing to compare against. Unreachable today because the log keyspace
        // is never truncated, and it becomes reachable the day retention reaches
        // it — at which point a node that was away long enough is exactly the
        // node this check was written for (Q-529).
        let Some(value) = stored else {
            return Ok(());
        };
        let held = LogRecord::epoch_in(value.as_slice())?;
        if held == offered {
            return Ok(());
        }
        // Asked here and nowhere earlier. Two leaderships at one position is a
        // divergence on a single-leader range and two masters on a declared
        // one, and the two arrive identically — so the declaration is what
        // tells them apart. Reading it costs a catalog lookup, which is why it
        // is behind the epoch comparison rather than in front of it: the branch
        // above is the ordinary retry, it is the common case by a wide margin,
        // and it pays nothing for this.
        if self.admits_two_writers(log.home)? {
            return Ok(());
        }
        self.divergences.fetch_add(1, Ordering::Relaxed);
        Err(Error::LogDivergence {
            sequence: at,
            held,
            offered,
        })
    }
}
