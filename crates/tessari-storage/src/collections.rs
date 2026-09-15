//! What this node knows about its own copy: when it last collected, and when it
//! was last **level**.
//!
//! # Collecting is not catching up
//!
//! A follower that asked five seconds ago and was handed a full answer has
//! proved that it asked. It has not proved that it arrived: if the leader held
//! more than the bound allowed, the copy is older than the contact, and a
//! staleness bound measured from the contact would admit exactly the read it was
//! written to exclude.
//!
//! Being level is observable, and it is observable exactly once — when the
//! answer comes back **shorter than the bound allowed**, including when it
//! carries nothing at all, because a leader serves `min(limit, available)`. A
//! short answer means the leader had no more to give, and that instant is when
//! this copy was current. Nothing else in a collection says so.
//!
//! This is the follower's side of the pair `crate::followers` describes on the
//! leader's: there, `quiet_for` is an age **only** while `behind` is zero. Same
//! rule, same reason, the other way round — and here the node cannot read
//! `behind`, because it has no idea what the leader went on to write.
//!
//! # Why none of this is persisted
//!
//! The argument `crate::followers` makes, and it is stronger here. A process
//! restored from a backup would read a file saying its copy was current four
//! seconds ago, and publish that after a week of being switched off. Unknown is
//! the answer that is true, and it is the answer that excludes.

use std::sync::Mutex;
use std::time::Instant;

use tessari_types::Sequence;

/// Whether a collection brought this node level with the peer it asked.
///
/// An enum rather than a `bool` because the two are read in opposite directions
/// and a bare `true` at a call site says which of them only by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    /// The answer was shorter than the bound allowed, so the peer had no more.
    Level,
    /// The answer filled the bound, so the peer may well have more.
    Behind,
}

/// The last thing this node learned about its own copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collection {
    /// The highest sequence this node has been given.
    pub reached: Sequence,
    /// When this node was last level with the peer it collects from.
    ///
    /// `None` means it has collected and never arrived — which is a copy of
    /// unknown age, not a copy of age zero.
    pub level_at: Option<Instant>,
}

/// What this node has collected, held for the life of the process.
#[derive(Debug, Default)]
pub struct Collections {
    last: Mutex<Option<Collection>>,
}

impl Collections {
    /// Record a collection that reached `reached` and was or was not level.
    ///
    /// Takes no result and returns none, for the reason
    /// [`crate::followers::Followers::served`] does not either: a registry that
    /// can refuse is one that can fail replication in order to protect a
    /// diagnostic.
    ///
    /// A `Behind` collection **keeps** an earlier `level_at` rather than
    /// clearing it. That is deliberate and it is the conservative direction:
    /// the copy was genuinely current at that instant, and it has only grown
    /// older since — which is what the age is measured as. Clearing it would
    /// make a follower that is steadily catching up report *unknown* forever,
    /// and unknown excludes it from every bounded read.
    pub fn collected(&self, reached: Sequence, currency: Currency) {
        if let Ok(mut last) = self.last.lock() {
            let level_at = match currency {
                Currency::Level => Some(Instant::now()),
                Currency::Behind => last.and_then(|held| held.level_at),
            };
            *last = Some(Collection { reached, level_at });
        }
    }

    /// The last collection, if this node has ever made one.
    #[must_use]
    pub fn last(&self) -> Option<Collection> {
        self.last.lock().ok().and_then(|last| *last)
    }
}
