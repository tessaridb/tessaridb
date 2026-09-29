//! One campaign round: the ballots out, the votes back, and the majority it needs.

use super::{Ballot, Leadership, Vote};
use std::time::Instant;
use tessari_encoding::NODE_ID_LEN;
use tessari_storage::LEASE_TTL;
use tessari_types::{Epoch, Reach};

/// A candidate's round: one epoch, asked of a known set of voters.
#[derive(Debug)]
pub struct Round {
    ballot: Ballot,
    voters: usize,
    opened: Instant,
    granted: Vec<[u8; NODE_ID_LEN]>,
}

impl Round {
    /// Open a round now.
    ///
    /// `voters` is the size of the configured voting set, which is what a
    /// majority is a majority *of*. A round opened against a set of zero can
    /// never conclude, which is the right answer rather than a special case.
    #[must_use]
    pub fn opened(epoch: Epoch, candidate: [u8; NODE_ID_LEN], voters: usize) -> Self {
        Self::opened_at(epoch, candidate, voters, Instant::now())
    }

    /// Open a round at a stated instant.
    #[must_use]
    pub fn opened_at(
        epoch: Epoch,
        candidate: [u8; NODE_ID_LEN],
        voters: usize,
        opened: Instant,
    ) -> Self {
        Self {
            ballot: Ballot {
                epoch,
                candidate,
                range: Reach::Store,
            },
            voters,
            opened,
            granted: Vec::new(),
        }
    }

    /// The same round, on a placed range's own line (ADR-0082).
    #[must_use]
    pub const fn over(mut self, range: Reach) -> Self {
        self.ballot.range = range;
        self
    }

    /// The ballot to put to every voter.
    #[must_use]
    pub const fn ballot(&self) -> Ballot {
        self.ballot
    }

    /// Record one voter's answer, and say whether that answer carried the round.
    ///
    /// A voter is counted once however many times it answers: a majority is a
    /// majority of *members*, and a transport that retried would otherwise be
    /// able to elect a leader on its own.
    pub fn counts(&mut self, voter: [u8; NODE_ID_LEN], vote: Vote) -> Option<Leadership> {
        if vote == Vote::Granted && !self.granted.contains(&voter) {
            self.granted.push(voter);
        }
        self.held()
    }

    /// How many more grants this round still needs.
    ///
    /// Zero when it is already carried. It exists so a candidate can tell
    /// whether its own ballot would decide anything before casting it — see
    /// [`crate::Standing::renew`], where casting it too early was a permanent
    /// outage rather than an inefficiency (ADR-0066).
    #[must_use]
    pub fn needs(&self) -> usize {
        majority(self.voters).saturating_sub(self.granted.len())
    }

    /// The grant this round has won, if it has won one.
    #[must_use]
    pub fn held(&self) -> Option<Leadership> {
        (self.granted.len() >= majority(self.voters)).then_some(Leadership {
            epoch: self.ballot.epoch,
            from: self.opened,
        })
    }
}

/// How many of `voters` it takes to carry a round.
///
/// Strictly more than half. Half exactly would let two disjoint halves of an
/// even set each carry a round of their own, which is the split-brain written as
/// an off-by-one.
#[must_use]
pub const fn majority(voters: usize) -> usize {
    voters.div_euclid(2).saturating_add(1)
}

/// One TTL after `at`, which is when a grant made then is certainly dead.
///
/// Saturating rather than wrapping for the reason [`Lease::taken_at`] floors its
/// own: an instant so far out that the addition cannot be represented is not a
/// reason to hand back an earlier one, and an earlier one here would free a
/// voter that is not free.
pub(crate) fn free_at(at: Instant) -> Instant {
    at.checked_add(LEASE_TTL).unwrap_or(at)
}
