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

use std::sync::{Mutex, MutexGuard, PoisonError};

/// The turn every in-process writer of log records takes.
#[derive(Debug, Default)]
pub(crate) struct WriteGate {
    turn: Mutex<()>,
}

impl WriteGate {
    /// Wait for this writer's turn, holding it until the guard is dropped.
    pub(crate) fn hold(&self) -> MutexGuard<'_, ()> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
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
