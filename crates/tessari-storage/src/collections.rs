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

/// Where this node stands against the peer it collects from (ADR-0094 D4).
///
/// The follower's own answer, because only the follower knows it is copying:
/// its leader sees one long read on the peer door and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// Its position is below the leader's log start, and it is copying the
    /// leader's state before it collects again.
    Copying,
    /// It collects, and the last answer filled the bound, so there is more.
    CatchingUp,
    /// The last answer was shorter than the bound: it holds what its leader
    /// had to give.
    InSync,
    /// A copy failed. The node is behind, not damaged: a failed copy writes
    /// no position, and the next round tries again.
    CopyFailed,
    /// Its position is below the leader's log start, and it leads something
    /// of its own, so a copy would overwrite what it is the origin of. An
    /// operator restores it from a snapshot; it is not re-seeded by itself.
    Stranded,
}

impl Upstream {
    /// Every state, in the order a scrape lists them.
    pub const ALL: [Self; 5] = [
        Self::Copying,
        Self::CatchingUp,
        Self::InSync,
        Self::CopyFailed,
        Self::Stranded,
    ];

    /// The name a report prints, in the words an operator reads.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Copying => "copying",
            Self::CatchingUp => "catching up",
            Self::InSync => "in sync",
            Self::CopyFailed => "copy failed",
            Self::Stranded => "stranded",
        }
    }
}

/// [`Upstream`], with what this process has copied so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamReport {
    /// Where the node stands now.
    pub state: Upstream,
    /// How many records the copies this process made have installed.
    pub copied_records: u64,
    /// How many copies this process has completed.
    pub copies: u64,
}

/// What this node has collected, held for the life of the process.
#[derive(Debug, Default)]
pub struct Collections {
    last: Mutex<Option<Collection>>,
    upstream: Mutex<Option<UpstreamReport>>,
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
        // A collection that landed ends a failed copy or a stranding: the
        // peer answered, so the node is following again.
        self.upstream_is(match currency {
            Currency::Level => Upstream::InSync,
            Currency::Behind => Upstream::CatchingUp,
        });
    }

    /// Record where this node now stands against its upstream.
    pub fn upstream_is(&self, state: Upstream) {
        if let Ok(mut held) = self.upstream.lock() {
            let (copied_records, copies) =
                held.map_or((0, 0), |held| (held.copied_records, held.copies));
            *held = Some(UpstreamReport {
                state,
                copied_records,
                copies,
            });
        }
    }

    /// Record a copy that installed `records` records; the node now collects
    /// from where the copy stood it.
    pub fn copied(&self, records: u64) {
        if let Ok(mut held) = self.upstream.lock() {
            let (copied_records, copies) =
                held.map_or((0, 0), |held| (held.copied_records, held.copies));
            *held = Some(UpstreamReport {
                state: Upstream::CatchingUp,
                copied_records: copied_records.saturating_add(records),
                copies: copies.saturating_add(1),
            });
        }
    }

    /// Where this node stands against its upstream, or `None` when it has
    /// never collected nor copied — a node that follows nobody.
    #[must_use]
    pub fn upstream(&self) -> Option<UpstreamReport> {
        self.upstream.lock().ok().and_then(|held| *held)
    }

    /// The last collection, if this node has ever made one.
    #[must_use]
    pub fn last(&self) -> Option<Collection> {
        self.last.lock().ok().and_then(|last| *last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_reads_catching_up_until_a_short_collection_says_level() {
        let held = Collections::default();
        assert_eq!(
            held.upstream(),
            None,
            "a node that never collected follows nobody yet"
        );
        held.upstream_is(Upstream::Copying);
        assert_eq!(
            held.upstream().map(|seen| seen.state),
            Some(Upstream::Copying)
        );
        held.copied(40);
        let seen = held.upstream().expect("a copy happened");
        assert_eq!(
            (seen.state, seen.copied_records, seen.copies),
            (Upstream::CatchingUp, 40, 1)
        );
        held.collected(Sequence::new(9), Currency::Behind);
        assert_eq!(
            held.upstream().map(|seen| seen.state),
            Some(Upstream::CatchingUp)
        );
        held.collected(Sequence::new(12), Currency::Level);
        let seen = held.upstream().expect("still known");
        assert_eq!((seen.state, seen.copied_records), (Upstream::InSync, 40));
    }

    #[test]
    fn a_collection_that_lands_ends_a_failed_or_stranded_state() {
        let held = Collections::default();
        for refused in [Upstream::CopyFailed, Upstream::Stranded] {
            held.upstream_is(refused);
            assert_eq!(held.upstream().map(|seen| seen.state), Some(refused));
            held.collected(Sequence::new(3), Currency::Level);
            assert_eq!(
                held.upstream().map(|seen| seen.state),
                Some(Upstream::InSync)
            );
        }
    }
}
