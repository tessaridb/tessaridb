//! One writer at a time inside this process, by queueing rather than racing.
//!
//! # Why writers queue here
//!
//! Every write that files a log record — a local commit and a replica's apply
//! alike — moves the store-wide version counter by one, and asserts the value it
//! read as a precondition of its batch. Two writers in this process that read the
//! same counter therefore cannot both land: one wins and the other's attempt is
//! thrown away with everything derived for it, then retried after a wait. A
//! commit that lost its bounded number of attempts was **refused** — for writes
//! that shared no record with anything — and measured on 2026-09-27 that was 258
//! of 3 200 commits with sixteen writers on disk, beginning at four.
//!
//! Racing was never buying concurrency. The counter admits one writer at a time
//! whatever happens, so the losers' work was pure waste and their waits were pure
//! latency. Holding this gate from reading the counter to applying the batch
//! makes the order explicit: a writer waits its turn once, and a turn it is given
//! cannot be lost to another writer in this process.
//!
//! # What still guards the counter
//!
//! The batch's own preconditions, unchanged. The engine admits one writing
//! process per store, so inside it this gate is sufficient — and the assertions
//! stay, because a guarantee that rests on every caller remembering a lock is
//! only as strong as the next caller.
//!
//! # A poisoned gate is still a gate
//!
//! It guards no data of its own. A writer that panicked while holding it left
//! nothing half-written here — its batch either reached the engine whole or did
//! not reach it — so the next writer takes the gate as it finds it rather than
//! refusing every write for the rest of the process's life.
//!
//! # A turn ends at staging, not at the device
//!
//! A commit holds the turn while it derives its batch and hands the batch to
//! [`pending`] rather than applying it, so the next writer derives while the
//! one before it is still being synced — and a group of them lands in one
//! write (G040 SG4). What a writer holding the turn reads includes what is
//! pending ([`Overlaid`]); what anybody else reads does not.
//!
//! # Every landing is announced from here
//!
//! A commit's landing, a batch applied under the turn and a replica's apply all
//! pass through this gate, so it is the one place that knows a log record has
//! become readable whichever surface, cadence or peer produced it. A follower of
//! the log waits on that announcement ([`WriteGate::when_landed`]) rather than
//! on a timer.

#[cfg(test)]
mod group_tests;
mod overlay;
#[cfg(test)]
mod overlay_tests;
mod pending;

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use tessari_kv::{KvBackend, WriteBatch};

pub(crate) use overlay::Overlaid;
pub(crate) use pending::Ticket;

thread_local! {
    /// The gate this thread holds the turn of, by address; zero for none.
    static HOLDING: Cell<usize> = const { Cell::new(0) };
}

/// The turn every in-process writer of log records takes.
#[derive(Debug, Default)]
pub(crate) struct WriteGate {
    turn: Mutex<()>,
    pending: pending::Pending,
    landed: Hooks,
}

/// What to call once a write that filed a log record has landed.
#[derive(Default)]
struct Hooks(RwLock<Vec<Box<dyn Fn() + Send + Sync>>>);

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

/// A held turn. Dropping it hands the turn on.
#[derive(Debug)]
pub(crate) struct Turn<'a> {
    _held: MutexGuard<'a, ()>,
    before: usize,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        HOLDING.with(|holding| holding.set(self.before));
    }
}

impl WriteGate {
    /// Wait for this writer's turn, holding it until the guard is dropped.
    pub(crate) fn hold(&self) -> Turn<'_> {
        let held = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        let before = HOLDING.with(|holding| holding.replace(self.address()));
        Turn {
            _held: held,
            before,
        }
    }

    /// Hand a derived batch over to be landed with whatever else is staged.
    /// Called holding the turn; the answer comes from [`Self::land`].
    pub(crate) fn stage(&self, batch: WriteBatch) -> Ticket {
        self.pending.stage(batch)
    }

    /// Wait for a staged batch to land, landing a group when nobody else is.
    /// Called after the turn is released.
    ///
    /// # Errors
    ///
    /// The batch's own failure, or the retryable conflict when it was derived
    /// on a batch that did not land.
    pub(crate) fn land(&self, ticket: Ticket, backend: &dyn KvBackend) -> tessari_kv::Result<()> {
        let landed = self.pending.land(ticket, backend);
        if landed.is_ok() {
            self.announce();
        }
        landed
    }

    /// Apply a batch that is not staged — a backend with no sync to share, or a
    /// replica's apply — and announce it once it has landed. Called holding the
    /// turn.
    ///
    /// # Errors
    ///
    /// The batch's own failure.
    pub(crate) fn apply(
        &self,
        batch: WriteBatch,
        backend: &dyn KvBackend,
    ) -> tessari_kv::Result<()> {
        let applied = backend.apply(batch);
        if applied.is_ok() {
            self.announce();
        }
        applied
    }

    /// Call `hook` after every write that files a log record lands, on the
    /// writer that landed it — after a commit's turn is handed on, but still
    /// holding it for an apply under the turn.
    ///
    /// Short and non-blocking by contract, and never taking the turn: it runs
    /// on the commit path.
    pub(crate) fn when_landed(&self, hook: Box<dyn Fn() + Send + Sync>) {
        self.landed
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hook);
    }

    fn announce(&self) {
        for hook in self
            .landed
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            hook();
        }
    }

    /// Land everything pending. For a writer holding the turn that applies its
    /// own batch — a replica's apply — so that it allocates from a store with
    /// nothing in flight.
    pub(crate) fn land_all(&self, backend: &dyn KvBackend) {
        self.pending.land_all(backend);
    }

    /// How many commits are staged and not yet in flight.
    #[cfg(test)]
    pub(crate) fn staged(&self) -> usize {
        self.pending.staged()
    }

    /// Whether this thread holds the turn, and so may read batches that have
    /// not landed.
    pub(crate) fn holding(&self) -> bool {
        self.held_here()
    }

    /// Whether this thread holds the turn.
    fn held_here(&self) -> bool {
        HOLDING.with(Cell::get) == self.address()
    }

    fn address(&self) -> usize {
        std::ptr::from_ref(self).addr()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use tessari_kv::{KvBackend, MemoryBackend};
    use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

    use crate::{Error, RecordAddress, Store};

    fn write(store: &Store) -> crate::Result<()> {
        let mut transaction = store.begin()?;
        transaction.put(
            RecordAddress::new(
                NamespaceId::new(1),
                DatabaseId::new(1),
                TableId::new(1),
                RecordId::from("one"),
            ),
            b"{}".to_vec(),
        );
        transaction.commit().map(|_| ())
    }

    /// Hold the turn, let a writer queue on it, run `meanwhile`, then let it go.
    fn queued_behind(store: &Store, meanwhile: impl FnOnce()) -> crate::Result<()> {
        let turn = store.write_gate().hold();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| write(store));
            // Long enough for the writer to pass admission and reach the gate;
            // admission is microseconds of in-memory reads.
            std::thread::sleep(Duration::from_millis(200));
            meanwhile();
            drop(turn);
            writer.join().unwrap()
        })
    }

    #[test]
    fn a_commit_that_waited_for_its_turn_writes_when_the_fence_is_still_open() {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        store.hold_lease(Duration::from_secs(60));
        queued_behind(&store, || {}).expect("the wait alone refuses nothing");
    }

    #[test]
    fn a_commit_that_waited_for_its_turn_is_judged_against_the_fence_it_meets() {
        // Admitted under a live lease, then queued while the fence shut: a
        // write landing now could land after another node was granted the
        // leadership, which is what the fence exists to prevent.
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        store.hold_lease(Duration::from_secs(60));
        let refused = queued_behind(&store, || store.hold_lease(Duration::ZERO));
        assert!(
            matches!(refused, Err(Error::LeaseSpent { .. })),
            "{refused:?}"
        );
    }
}
