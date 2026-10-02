//! The node's one voting memory, shared by the door and the campaign.

use super::{Ballot, Reached, Vote, Voter};
use std::time::Instant;
use tessari_encoding::NODE_ID_LEN;
use tessari_types::{Epoch, Reach};

/// One node's voting memory, reachable from every thread that can need it.
///
/// # Why the memory is shared rather than owned by the door
///
/// A node votes in two places. A peer's ballot arrives at the door; this node's
/// own ballot, when it stands for an epoch of its own, is decided at home. Both
/// are votes, and a voter grants an epoch **at most once** — which is the whole
/// safety argument [`Voter`] rests on and it is a statement about the node, not
/// about a variable.
///
/// A campaign that counted its own vote without recording it here would leave
/// this node free to grant the same epoch to somebody else a moment later. Two
/// candidates would then hold one epoch, each with an honest majority, and
/// nothing anywhere would be in an error state. That is the split-brain this
/// module opens by declaring impossible, arriving through the one voter the
/// design forgot was also a candidate.
///
/// # The lock is taken to decide a vote, never to wait for one
///
/// [`crate::Peers::greet`] waits inside `accept`, so a door holding this lock
/// across a whole call would block a campaign for as long as no peer happened to
/// ring. It takes the lock where the vote is actually decided instead, which is
/// a few comparisons long.
#[derive(Debug)]
pub struct Deciding {
    voter: std::sync::Mutex<Voter>,
    /// One memory per placed range's line (ADR-0082), each started at the
    /// store line's start instant: a process that restarted cannot remember a
    /// grant on ANY line, so the restart guard covers every one of them.
    lines: std::sync::Mutex<std::collections::BTreeMap<Reach, Voter>>,
}

impl Deciding {
    /// A voting memory that started now.
    #[must_use]
    pub fn started() -> Self {
        Self::holding(Voter::started())
    }

    /// A voting memory around a voter whose start instant the caller stated.
    #[must_use]
    pub fn holding(voter: Voter) -> Self {
        Self {
            voter: std::sync::Mutex::new(voter),
            lines: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Answer one ballot, from wherever it came.
    ///
    /// # A poisoned lock recovers rather than refusing
    ///
    /// The same decision [`crate::Published`] takes and for the same reason: what
    /// the lock protects is one `Option` assigned whole, so a thread that
    /// panicked mid-call left a complete memory behind and never half of one.
    /// Refusing to read it would take this node out of every round for the rest
    /// of the process's life over a panic elsewhere — a permanent availability
    /// loss bought with no safety, because the value being guarded is sound.
    pub fn asked(&self, ballot: &Ballot, now: Instant, mine: Reached, candidate: Reached) -> Vote {
        if ballot.range == Reach::Store {
            return self.held().asked(ballot, now, mine, candidate);
        }
        let started = self.held().started;
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lines
            .entry(ballot.range)
            .or_insert_with(|| Voter::started_at(started))
            .asked(ballot, now, mine, candidate)
    }

    /// Take a round a majority carried for this node, on the ballot's line —
    /// see [`Voter::carried`].
    ///
    /// # Errors
    ///
    /// The higher epoch this node already granted on that line.
    pub fn carried(&self, ballot: &Ballot, now: Instant) -> std::result::Result<(), Epoch> {
        if ballot.range == Reach::Store {
            return self.held().carried(ballot, now);
        }
        let started = self.held().started;
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(ballot.range)
            .or_insert_with(|| Voter::started_at(started))
            .carried(ballot, now)
    }

    /// When this node last granted a ballot to somebody else — see
    /// [`Voter::granted_elsewhere_at`].
    #[must_use]
    pub fn granted_elsewhere_at(&self, me: [u8; NODE_ID_LEN]) -> Option<Instant> {
        self.held().granted_elsewhere_at(me)
    }

    /// The same question on one placed range's line (ADR-0082) — `None` for a
    /// line this node has never been asked about.
    #[must_use]
    pub fn granted_elsewhere_on(&self, range: Reach, me: [u8; NODE_ID_LEN]) -> Option<Instant> {
        if range == Reach::Store {
            return self.granted_elsewhere_at(me);
        }
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&range)
            .and_then(|voter| voter.granted_elsewhere_at(me))
    }

    /// The highest epoch this node has granted, if any.
    #[must_use]
    pub fn decided(&self) -> Option<Epoch> {
        self.held().decided()
    }

    pub(crate) fn held(&self) -> std::sync::MutexGuard<'_, Voter> {
        self.voter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
