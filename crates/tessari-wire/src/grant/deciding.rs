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
    /// Called when this node grants the store line to a new epoch.
    granted_anew: std::sync::OnceLock<Announce>,
}

/// What [`Deciding::when_granted_anew`] calls.
pub type GrantedAnew = Box<dyn Fn() + Send + Sync>;

/// [`GrantedAnew`], held: a closure has nothing to print but that it is one.
struct Announce(GrantedAnew);

impl std::fmt::Debug for Announce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Announce")
    }
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
            granted_anew: std::sync::OnceLock::new(),
        }
    }

    /// Hold every grant made from now on for `hold`, on every line — the lease
    /// the store's installed failover policy states (G053 SG2c). Set at start
    /// and on every leadership pass, so a policy reaches the voter in the pass
    /// after it is installed.
    pub fn hold_for(&self, hold: std::time::Duration) {
        self.held().hold_for(hold);
        for voter in self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            voter.hold_for(hold);
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
            let (vote, before, after) = {
                let mut voter = self.held();
                let before = voter.granted_epoch();
                let vote = voter.asked(ballot, now, mine, candidate);
                (vote, before, voter.granted_epoch())
            };
            // A grant to a new epoch, not a renewal: a new leadership exists,
            // and this node knows it before any greeting could tell it.
            if after != before
                && let Some(Announce(granted_anew)) = self.granted_anew.get()
            {
                granted_anew();
            }
            return vote;
        }
        let (started, hold) = {
            let store = self.held();
            (store.started, store.hold)
        };
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lines
            .entry(ballot.range)
            .or_insert_with(|| Voter::started_at(started).holding_for(hold))
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
        let (started, hold) = {
            let store = self.held();
            (store.started, store.hold)
        };
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(ballot.range)
            .or_insert_with(|| Voter::started_at(started).holding_for(hold))
            .carried(ballot, now)
    }

    /// Call `granted_anew` every time this node grants the store line to a new
    /// epoch, outside the voter's lock — once set, for the life of the memory.
    ///
    /// The voter is the first part of a node to learn that a leadership
    /// changed: it granted it. A follower that waited for its next greeting to
    /// find out followed nobody for up to an awareness interval after every
    /// failover, while the new leader's first writes waited for its copies
    /// (Q-900). Answers `false`, and changes nothing, when one was already set.
    pub fn when_granted_anew(&self, granted_anew: GrantedAnew) -> bool {
        self.granted_anew.set(Announce(granted_anew)).is_ok()
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
