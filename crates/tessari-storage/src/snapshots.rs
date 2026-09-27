//! Which snapshots are still being read from.
//!
//! This store's MVCC is its own: a snapshot is a sequence number, versions live
//! inline under a record key, and a read seeks to the newest version at or below
//! the reader's sequence. Nothing in the engine below knows any of that, so
//! nothing in the engine below can tell reclamation which old versions are still
//! needed. This registry is how the store knows.
//!
//! # The failure this is shaped around
//!
//! The retention floor is bounded by the **oldest live snapshot**. A transaction
//! that registers and never releases therefore freezes reclamation for the life
//! of the process — not loudly, but as space that never comes back and reads
//! that get slower forever. So release is not something a caller does: it
//! happens in [`Drop`], which runs whether the transaction was committed, rolled
//! back, or simply let go of.
//!
//! # Why it is split into shards
//!
//! Every statement begins and drops a transaction, and one that checks access
//! begins two, so every reader registers and releases here at least twice per
//! statement. Behind one lock that was the point where concurrent readers
//! queued: profiled on 2026-09-28 with sixteen readers on disk, half of every
//! reader's time was spent waiting for this lock (G040 SG5). Each thread now
//! registers in a shard of its own, spread round-robin as threads first arrive,
//! and a transaction remembers the shard it registered in so that releasing it
//! from another thread still finds its entry. The rare questions about the
//! whole set — the oldest snapshot, its age, how many there are — read every
//! shard, each under its own lock, so an entry held for the whole of the call is
//! always seen.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tessari_types::Sequence;

/// How many shards the registry is split into.
const SHARDS: usize = 16;

/// The next shard a newly seen thread is given.
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The shard this thread registers in, once it has registered anything.
    static SHARD: Cell<Option<usize>> = const { Cell::new(None) };
}

/// One live snapshot: how many readers hold it, and since when.
#[derive(Debug)]
struct Held {
    holders: usize,
    since: Instant,
}

/// Where a snapshot was registered, to be handed back when it is released.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot(usize);

/// The snapshots currently being read from.
///
/// Keyed by sequence within each shard, so two transactions that began at the
/// same committed tail on one shard share one entry and the second to finish is
/// the one that frees it.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    shards: [Shard; SHARDS],
}

/// One shard, alone on its cache line: threads are given neighbouring shards,
/// and two shards sharing a line would pass it between cores on every
/// register and release — measured as two to four readers running slower than
/// under one lock.
#[derive(Debug, Default)]
#[repr(align(128))]
struct Shard(Mutex<BTreeMap<Sequence, Held>>);

impl Registry {
    /// Record that a reader is working at `at`, in this thread's shard.
    pub(crate) fn register(&self, at: Sequence) -> Slot {
        let slot = home();
        self.shard(slot)
            .entry(at)
            .and_modify(|held| held.holders = held.holders.saturating_add(1))
            .or_insert_with(|| Held {
                holders: 1,
                since: Instant::now(),
            });
        Slot(slot)
    }

    /// Record that one reader at `at` has finished.
    ///
    /// Releasing a sequence that is not registered in that shard is a no-op
    /// rather than a panic: it runs inside `Drop`, where a panic would abort
    /// during unwinding.
    pub(crate) fn release(&self, at: Sequence, slot: Slot) {
        let mut live = self.shard(slot.0);
        let Some(held) = live.get_mut(&at) else {
            return;
        };
        held.holders = held.holders.saturating_sub(1);
        if held.holders == 0 {
            live.remove(&at);
        }
    }

    /// The oldest sequence any live reader is working at.
    pub(crate) fn oldest(&self) -> Option<Sequence> {
        (0..SHARDS)
            .filter_map(|slot| self.shard(slot).keys().next().copied())
            .min()
    }

    /// How long the longest-held live snapshot has been held.
    pub(crate) fn oldest_age(&self) -> Option<Duration> {
        (0..SHARDS)
            .filter_map(|slot| {
                self.shard(slot)
                    .values()
                    .map(|held| held.since.elapsed())
                    .max()
            })
            .max()
    }

    /// How many distinct sequences are being read from.
    pub(crate) fn len(&self) -> usize {
        let mut distinct = BTreeSet::new();
        for slot in 0..SHARDS {
            distinct.extend(self.shard(slot).keys().copied());
        }
        distinct.len()
    }

    /// One shard, read past a poisoned lock: every change under it is a single
    /// map operation, so a panic elsewhere cannot have left it half-changed, and
    /// refusing it would stop every reader in the store for the rest of the
    /// process.
    fn shard(&self, slot: usize) -> MutexGuard<'_, BTreeMap<Sequence, Held>> {
        self.shards
            .get(slot)
            .unwrap_or(&self.shards[0])
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// This thread's shard, assigned round-robin the first time it registers.
fn home() -> usize {
    SHARD.with(|shard| {
        shard.get().unwrap_or_else(|| {
            let assigned = NEXT_SHARD
                .fetch_add(1, Ordering::Relaxed)
                .checked_rem(SHARDS)
                .unwrap_or(0);
            shard.set(Some(assigned));
            assigned
        })
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_panic_while_the_registry_was_held_does_not_hide_a_live_reader() {
        let registry = Registry::default();
        let five = registry.register(Sequence::new(5));
        let poisoned = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _held: Vec<_> =
                        registry.shards.iter().map(|shard| shard.0.lock()).collect();
                    std::panic::resume_unwind(Box::new("a defect while holding the registry"));
                })
                .join()
        });
        assert!(poisoned.is_err(), "the helper thread must have panicked");
        assert!(registry.shards.iter().all(|shard| shard.0.is_poisoned()));

        let three = registry.register(Sequence::new(3));
        assert_eq!(
            registry.oldest(),
            Some(Sequence::new(3)),
            "a reader registered after the panic still bounds the floor"
        );
        registry.release(Sequence::new(3), three);
        assert_eq!(registry.oldest(), Some(Sequence::new(5)));
        assert_eq!(registry.len(), 1);
        assert!(registry.oldest_age().is_some());
        registry.release(Sequence::new(5), five);
    }

    #[test]
    fn the_oldest_live_snapshot_is_what_bounds_the_floor() {
        let registry = Registry::default();
        assert_eq!(registry.oldest(), None);
        registry.register(Sequence::new(10));
        let four = registry.register(Sequence::new(4));
        registry.register(Sequence::new(7));
        assert_eq!(registry.oldest(), Some(Sequence::new(4)));
        registry.release(Sequence::new(4), four);
        assert_eq!(registry.oldest(), Some(Sequence::new(7)));
    }

    #[test]
    fn two_readers_at_one_sequence_share_an_entry_and_the_last_one_frees_it() {
        let registry = Registry::default();
        let first = registry.register(Sequence::new(3));
        let second = registry.register(Sequence::new(3));
        assert_eq!(registry.len(), 1);
        registry.release(Sequence::new(3), first);
        assert_eq!(
            registry.oldest(),
            Some(Sequence::new(3)),
            "one reader is still working at it"
        );
        registry.release(Sequence::new(3), second);
        assert_eq!(registry.oldest(), None);
    }

    #[test]
    fn releasing_something_that_was_never_registered_changes_nothing() {
        let registry = Registry::default();
        let slot = registry.register(Sequence::new(5));
        registry.release(Sequence::new(99), slot);
        assert_eq!(registry.oldest(), Some(Sequence::new(5)));
    }

    #[test]
    fn a_snapshot_released_on_another_thread_is_released() {
        // A transaction may be dropped on a thread other than the one that began
        // it; its entry is found through what registering returned, not through
        // whichever thread happens to drop it.
        let registry = Registry::default();
        let slot = std::thread::scope(|scope| {
            scope
                .spawn(|| registry.register(Sequence::new(8)))
                .join()
                .unwrap()
        });
        assert_eq!(registry.oldest(), Some(Sequence::new(8)));
        registry.release(Sequence::new(8), slot);
        assert_eq!(registry.oldest(), None, "the entry outlived its release");
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn readers_on_many_threads_all_bound_the_floor() {
        let registry = Registry::default();
        let slots: Vec<_> = std::thread::scope(|scope| {
            (1..=40_u64)
                .map(|at| {
                    let registry = &registry;
                    scope.spawn(move || (at, registry.register(Sequence::new(at))))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(registry.oldest(), Some(Sequence::new(1)));
        assert_eq!(registry.len(), 40);
        for (at, slot) in slots {
            if at < 40 {
                registry.release(Sequence::new(at), slot);
            }
        }
        assert_eq!(registry.oldest(), Some(Sequence::new(40)));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn an_age_exists_only_while_something_is_held() {
        let registry = Registry::default();
        assert_eq!(registry.oldest_age(), None);
        let slot = registry.register(Sequence::new(1));
        assert!(registry.oldest_age().is_some());
        registry.release(Sequence::new(1), slot);
        assert_eq!(registry.oldest_age(), None);
    }
}
