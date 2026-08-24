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

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tessari_types::Sequence;

/// One live snapshot: how many readers hold it, and since when.
#[derive(Debug)]
struct Held {
    holders: usize,
    since: Instant,
}

/// The snapshots currently being read from.
///
/// Keyed by sequence, so two transactions that began at the same committed tail
/// share one entry and the second to finish is the one that frees it.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    live: Mutex<BTreeMap<Sequence, Held>>,
}

impl Registry {
    /// Record that a reader is working at `at`.
    pub(crate) fn register(&self, at: Sequence) {
        let Ok(mut live) = self.live.lock() else {
            // A poisoned registry means a panic while holding it. Failing to
            // register would let reclamation run past a live reader, so the
            // safe direction is to leave the floor where it is.
            return;
        };
        live.entry(at)
            .and_modify(|held| held.holders = held.holders.saturating_add(1))
            .or_insert_with(|| Held {
                holders: 1,
                since: Instant::now(),
            });
    }

    /// Record that a reader at `at` has finished.
    pub(crate) fn release(&self, at: Sequence) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        let Some(held) = live.get_mut(&at) else {
            return;
        };
        held.holders = held.holders.saturating_sub(1);
        if held.holders == 0 {
            live.remove(&at);
        }
    }

    /// The oldest sequence any live reader still needs.
    pub(crate) fn oldest(&self) -> Option<Sequence> {
        let live = self.live.lock().ok()?;
        live.keys().next().copied()
    }

    /// How long the oldest live snapshot has been held.
    ///
    /// The value ADR-0005 §9 calls an operational limit. Without it, "a long-held
    /// snapshot postpones every tombstone in the store" is a sentence nobody can
    /// act on.
    pub(crate) fn oldest_age(&self) -> Option<Duration> {
        let live = self.live.lock().ok()?;
        live.values().map(|held| held.since.elapsed()).max()
    }

    /// How many distinct snapshots are live.
    pub(crate) fn len(&self) -> usize {
        self.live.lock().map_or(0, |live| live.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_live_snapshot_is_what_bounds_the_floor() {
        let registry = Registry::default();
        assert_eq!(registry.oldest(), None);

        registry.register(Sequence::new(10));
        registry.register(Sequence::new(4));
        registry.register(Sequence::new(7));
        assert_eq!(registry.oldest(), Some(Sequence::new(4)));

        registry.release(Sequence::new(4));
        assert_eq!(registry.oldest(), Some(Sequence::new(7)));
    }

    #[test]
    fn two_readers_at_one_sequence_share_an_entry_and_the_last_one_frees_it() {
        let registry = Registry::default();
        registry.register(Sequence::new(3));
        registry.register(Sequence::new(3));
        assert_eq!(registry.len(), 1);

        registry.release(Sequence::new(3));
        assert_eq!(
            registry.oldest(),
            Some(Sequence::new(3)),
            "one reader is still working at it"
        );

        registry.release(Sequence::new(3));
        assert_eq!(registry.oldest(), None);
    }

    #[test]
    fn releasing_something_that_was_never_registered_changes_nothing() {
        let registry = Registry::default();
        registry.register(Sequence::new(5));
        registry.release(Sequence::new(99));
        assert_eq!(registry.oldest(), Some(Sequence::new(5)));
    }

    #[test]
    fn an_age_exists_only_while_something_is_held() {
        let registry = Registry::default();
        assert_eq!(registry.oldest_age(), None);
        registry.register(Sequence::new(1));
        assert!(registry.oldest_age().is_some());
        registry.release(Sequence::new(1));
        assert_eq!(registry.oldest_age(), None);
    }
}
