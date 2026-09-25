//! The ranges this node leads on election lines of their own (ADR-0082).
//!
//! The store line keeps its own lease and epoch in [`crate::lease::Held`] and
//! `Store::leading`, untouched: a store with no placement never writes here. A
//! placed range is a separate line — its own epoch sequence, its own lease — and
//! this is where a node remembers the ones it won.
//!
//! Held in memory for the reason the store line's lease is: a persisted fence is
//! an unreplicated file asserting a cluster-wide fact.
//!
//! # A poisoned lock reads as spent
//!
//! The fence's decision, for the fence's reason: what the lock guards is whole
//! values assigned whole, but a gate that fails open on a panic elsewhere admits
//! a write nothing authorized, and a gate that fails closed costs one refusal.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tessari_types::{Epoch, Reach};

use crate::lease::Lease;

/// Every range line this node holds, with the epoch and lease it won it at.
#[derive(Debug, Default)]
pub(crate) struct Lines {
    held: Mutex<BTreeMap<Reach, (Epoch, Lease)>>,
}

/// Where this node stands on one range line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// No round on this line was ever won here.
    NotHeld,
    /// Held, and the fence is still open.
    Live,
    /// Held, and the fence shut this long ago.
    Spent(Duration),
}

impl Lines {
    /// Install a lease a majority granted on `range`'s line, exactly as granted.
    pub(crate) fn hold(&self, range: Reach, epoch: Epoch, lease: Lease) {
        if let Ok(mut held) = self.held.lock() {
            held.insert(range, (epoch, lease));
        }
    }

    /// The epoch this node holds `range`'s line at, while its lease is live.
    ///
    /// A lapsed line answers `None`, unlike the store line's epoch, which
    /// outlives its lease: a node knows its own line has lapsed, and a greeting
    /// advertising a leadership it can no longer write under would keep every
    /// other candidate for the range from standing (ADR-0066 per line).
    pub(crate) fn epoch_of(&self, range: Reach) -> Option<Epoch> {
        let held = self.held.lock().ok()?;
        let (epoch, lease) = held.get(&range)?;
        (!lease.fenced(Instant::now())).then_some(*epoch)
    }

    /// Whether this node may write on `range`'s line now.
    pub(crate) fn standing(&self, range: Reach) -> Standing {
        let Ok(held) = self.held.lock() else {
            return Standing::Spent(Duration::ZERO);
        };
        match held.get(&range) {
            None => Standing::NotHeld,
            Some((_, lease)) => {
                let now = Instant::now();
                if lease.fenced(now) {
                    Standing::Spent(lease.spent_for(now))
                } else {
                    Standing::Live
                }
            }
        }
    }

    /// Whether this node holds any range line at all.
    pub(crate) fn any(&self) -> bool {
        self.held.lock().map_or(true, |held| !held.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessari_types::{DatabaseId, NamespaceId, ShardId, TableId};

    fn shard(n: u32) -> Reach {
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            ShardId::new(n),
        )
    }

    #[test]
    fn a_line_is_held_per_range_and_a_lapsed_one_reads_as_spent() {
        let lines = Lines::default();
        assert!(!lines.any());
        assert_eq!(lines.standing(shard(1)), Standing::NotHeld);
        lines.hold(
            shard(1),
            Epoch::new(3),
            Lease::taken(Duration::from_secs(60)),
        );
        lines.hold(shard(2), Epoch::new(7), Lease::taken(Duration::ZERO));
        assert!(lines.any());
        assert_eq!(lines.standing(shard(1)), Standing::Live);
        assert!(matches!(lines.standing(shard(2)), Standing::Spent(_)));
        assert_eq!(lines.standing(shard(3)), Standing::NotHeld);
        assert_eq!(lines.epoch_of(shard(1)), Some(Epoch::new(3)));
        assert_eq!(
            lines.epoch_of(shard(2)),
            None,
            "a lapsed line leads nothing"
        );
        assert_eq!(lines.epoch_of(shard(3)), None);
    }
}
