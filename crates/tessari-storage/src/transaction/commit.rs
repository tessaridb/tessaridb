//! Writing, committing, and losing the race.
//!
//! A commit is the only place this type touches the store, and the only place a
//! conflict can be raised. The backoff below is what a loser does before trying
//! again: randomised, so two threads that lost the same race do not re-enter it
//! together.

use std::cell::Cell;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use tessari_constants::{COMMIT_BACKOFF_CEILING, COMMIT_BACKOFF_STEP, MAX_COMMIT_ATTEMPTS};
use tessari_encoding::{LogRecord, Mutation, RecordValue};
use tessari_types::Sequence;

use super::{RecordAddress, Transaction};
use crate::error::{Error, Result};

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
        if self.writes.is_empty() {
            return Ok(self.snapshot);
        }
        let record = self.log_record();

        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            if attempt > MAX_COMMIT_ATTEMPTS {
                return Err(Error::CommitContention {
                    attempts: MAX_COMMIT_ATTEMPTS,
                });
            }

            let tail = self.store.committed_tail()?;
            self.check_for_conflicts()?;
            // Inside the loop with the conflict check, and for the same reason:
            // both are read against the committed state this attempt builds on,
            // and a schema that moved between attempts must be re-read rather
            // than assumed.
            crate::schema::validate(self.store, &record)?;

            // Deciding the sequence locally is the *only* thing a commit does
            // that a replica's apply does not. Everything after this line is the
            // shared path.
            let commit_at = Sequence::new(tail.get().saturating_add(1));
            // Index entries are derived here rather than carried in the record,
            // and they are derived inside the loop because they depend on the
            // committed state this attempt is building on (see `crate::index`).
            let batch = crate::index::maintain(
                self.store,
                &record,
                crate::log::apply_batch(commit_at, &record),
            )?;
            match self.store.backend().apply(batch) {
                Ok(()) => return Ok(commit_at),
                // The position moved between reading it and applying, so the
                // conflict check above was made against a stale state and the
                // whole attempt is repeated rather than patched up — after
                // waiting, so that this attempt does not re-race into the same
                // instant as every other loser.
                Err(tessari_kv::Error::Conflict { .. }) => {
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
    fn log_record(&self) -> LogRecord {
        LogRecord::new(
            self.writes
                .iter()
                .map(|(address, value)| Mutation {
                    namespace: address.namespace,
                    database: address.database,
                    table: address.table,
                    id: address.id.clone(),
                    value: value.clone(),
                })
                .collect(),
        )
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
