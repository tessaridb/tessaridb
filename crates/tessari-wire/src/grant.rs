//! How a node comes to hold leadership, and why one voter's memory is enough.
//!
//! # The half that was missing
//!
//! The storage engine holds the **fence**: a leader stops writing at `T` while
//! the cluster may not reassign the leadership until `T + δ`. That is the
//! holder's promise, and on its own it promises nothing, because a node can
//! promise to stop writing at a time nobody else agreed to. This module is the
//! other end of the same pair — the rules by which a set of voting members
//! agrees that one candidate, and no other, holds a given epoch.
//!
//! # One grant per epoch is the whole safety argument
//!
//! Two leaders in one epoch are impossible because a voter grants an epoch at
//! most once and never grants an epoch it has already passed. Every other rule
//! here exists to stop that one from being circumvented by time.
//!
//! # The round is dated from before it asked
//!
//! The subtle one. Each voter's own hold runs from the instant **it** granted.
//! If a candidate dated its lease from the moment the majority came back, a slow
//! round would put the holder's fence *after* the earliest voter was already
//! free to grant again — which is the split-brain the guard exists to prevent,
//! arriving through the collection delay rather than through a clock.
//!
//! So a round stamps the instant it opened **before** the first ballot leaves,
//! and that is the instant the lease is taken at. Every millisecond spent
//! collecting comes out of the leader's own window and never out of the voters'.
//! A round that takes longer than the guard therefore yields a lease that is
//! already fenced, which is the safe direction: it refuses writes rather than
//! granting a window nobody bounded.
//!
//! # A restarted voter waits out what it cannot remember
//!
//! Nothing here is persisted, for the reason the fence gives: an unreplicated
//! file asserting a cluster-wide fact **is** the split-brain, and a persisted
//! vote is that claim with a backup copy. The cost is that a voter which has
//! just started may have granted a lease it has forgotten. It therefore refuses
//! to vote until it has been up for one full TTL — one TTL of reduced
//! availability after a restart, in exchange for exactly the safety a written
//! vote would have bought, and without a file that can be restored from a backup
//! and vote a second time for an epoch it already decided.
//!
//! # A refusal is an answer, not a failure
//!
//! Refusals are values rather than errors. A voter declining an epoch is the
//! mechanism working — most ballots in a healthy cluster are refused, because
//! the incumbent's lease is still alive — and a type that made every refusal an
//! error would put the normal case on the failure path.
//!
//! # What is not here
//!
//! **No frames and no socket.** How a ballot reaches a voter is not this
//! module's business, exactly as the handshake's rules never learn which
//! transport proved the identity they are handed. That seam is what let the
//! handshake be written before the transport decision was taken and survive it
//! unchanged.
//!
//! **No campaign.** Nothing decides *when* to stand, nothing renews on a timer,
//! and nothing checks whether a candidate's log is complete enough to lead. Those
//! are the next wave's, and they are policy over these rules rather than changes
//! to them.

use std::time::{Duration, Instant};

use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{LEASE_TTL, Lease};
use tessari_types::Epoch;

/// What a candidate asks each voting member for.
///
/// It names an epoch and a candidate and nothing else — in particular it does
/// not name a duration, because a candidate that could ask for its own TTL could
/// ask for a long one, and the two ends of a lease have to mean the same span by
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ballot {
    /// The leadership being asked for.
    pub epoch: Epoch,
    /// Who is asking.
    pub candidate: [u8; NODE_ID_LEN],
}

/// Why a voter said no.
///
/// Three refusals rather than one, because they send an operator somewhere
/// different: an epoch already decided means a candidate is re-running a round
/// that concluded; a grant still alive is the healthy steady state; and a voter
/// too recently started is a node that has restarted and is deliberately sitting
/// out one lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refused {
    /// This voter has already granted this epoch, or a later one.
    EpochAlreadyDecided {
        /// The highest epoch this voter has granted.
        granted: Epoch,
    },
    /// A grant this voter made may still be alive.
    EarlierGrantStillAlive {
        /// How long until this voter is free to grant again.
        for_the_next: Duration,
    },
    /// This voter has not been up long enough to know what it granted before it
    /// restarted.
    TooSoonAfterStarting {
        /// How long until it has outlived anything it may have forgotten.
        for_the_next: Duration,
    },
}

/// A voting member's answer to one ballot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    /// This voter will not grant this epoch to anyone else.
    Granted,
    /// It will not grant this one, and says which rule stopped it.
    Refused(Refused),
}

/// One voting member's memory of what it has granted.
///
/// Deliberately tiny: the highest epoch it granted and when. Those two facts are
/// the entire state a voter needs, which is why the restart rule can substitute
/// for persisting them.
#[derive(Debug)]
pub struct Voter {
    started: Instant,
    granted: Option<(Epoch, Instant)>,
}

impl Voter {
    /// A voter that started now.
    #[must_use]
    pub fn started() -> Self {
        Self::started_at(Instant::now())
    }

    /// A voter that started at a stated instant.
    ///
    /// The instant is taken rather than read so that the restart rule can be
    /// exercised without a clock, and therefore without a test that can flake.
    #[must_use]
    pub const fn started_at(started: Instant) -> Self {
        Self {
            started,
            granted: None,
        }
    }

    /// Answer one ballot.
    ///
    /// The checks are ordered by how different their remedies are. A repeated
    /// epoch is a confused candidate; a live grant is the normal answer and the
    /// caller wants to know how long to wait; a recent start is a node sitting
    /// out a lease it cannot remember.
    pub fn asked(&mut self, ballot: &Ballot, now: Instant) -> Vote {
        if let Some((granted, at)) = self.granted {
            if ballot.epoch <= granted {
                return Vote::Refused(Refused::EpochAlreadyDecided { granted });
            }
            let free = free_at(at);
            if now < free {
                return Vote::Refused(Refused::EarlierGrantStillAlive {
                    for_the_next: free.saturating_duration_since(now),
                });
            }
        } else {
            // Granted nothing since it started, so it cannot rule out having
            // granted something before it started.
            let settled = free_at(self.started);
            if now < settled {
                return Vote::Refused(Refused::TooSoonAfterStarting {
                    for_the_next: settled.saturating_duration_since(now),
                });
            }
        }

        self.granted = Some((ballot.epoch, now));
        Vote::Granted
    }

    /// The highest epoch this voter has granted, if any.
    #[must_use]
    pub fn decided(&self) -> Option<Epoch> {
        self.granted.map(|(epoch, _)| epoch)
    }

    /// When this voter is next free to grant, if it has granted at all.
    ///
    /// The grantor's **expiry**, not the holder's fence: the guard is exactly the
    /// difference between the two, and it belongs on this side of the pair.
    #[must_use]
    pub fn free_at(&self) -> Option<Instant> {
        self.granted.map(|(_, at)| free_at(at))
    }
}

/// Leadership a majority agreed to.
///
/// Named for the thing rather than for the holding, because the storage engine
/// already has a `Held` meaning *the lease this process is writing under* — one
/// name for two different facts in one workspace is a readability trap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leadership {
    /// The epoch the majority granted.
    pub epoch: Epoch,
    /// When the round that won it opened — see the module header for why this,
    /// and not the instant the majority answered, is what the lease is dated
    /// from.
    pub from: Instant,
}

impl Leadership {
    /// The lease this grant entitles the holder to.
    ///
    /// Dated from [`Leadership::from`], so a round that took a long time hands its
    /// holder a shorter window rather than one that outlives the voters'.
    #[must_use]
    pub fn lease(&self) -> Lease {
        Lease::taken_at(self.from, LEASE_TTL)
    }
}

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
            ballot: Ballot { epoch, candidate },
            voters,
            opened,
            granted: Vec::new(),
        }
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
fn free_at(at: Instant) -> Instant {
    at.checked_add(LEASE_TTL).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use super::{Ballot, Leadership, Refused, Round, Vote, Voter, majority};
    use std::time::{Duration, Instant};
    use tessari_encoding::NODE_ID_LEN;
    use tessari_storage::{LEASE_GUARD, LEASE_TTL};
    use tessari_types::Epoch;

    /// A base far enough ahead that every test can subtract from it without
    /// depending on how long this machine has been up.
    fn base() -> Instant {
        after(Instant::now(), Duration::from_secs(3600))
    }

    /// Checked throughout, because the workspace denies loose arithmetic
    /// everywhere and a test is not an exception to a rule about overflow.
    fn after(at: Instant, by: Duration) -> Instant {
        at.checked_add(by).expect("representable")
    }

    /// A voter that has been up long enough to have outlived anything it might
    /// have granted before a restart.
    fn settled(at: Instant) -> Voter {
        Voter::started_at(at.checked_sub(LEASE_TTL).expect("representable"))
    }

    const A: [u8; NODE_ID_LEN] = [0xA1; NODE_ID_LEN];
    const B: [u8; NODE_ID_LEN] = [0xB2; NODE_ID_LEN];
    const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
    const TWO: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
    const THREE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

    #[test]
    fn two_candidates_cannot_both_carry_one_epoch() {
        let now = base();
        let mut voters = [settled(now), settled(now), settled(now)];

        let mut first = Round::opened_at(Epoch::new(1), A, voters.len(), now);
        let mut held = None;
        for (voter, id) in voters.iter_mut().zip([ONE, TWO, THREE]) {
            let vote = voter.asked(&first.ballot(), now);
            held = first.counts(id, vote);
        }
        assert!(
            held.is_some(),
            "three willing voters carry a round of three"
        );

        // The second candidate asks the same epoch of the same voters, a
        // moment later, and every one of them has already decided it.
        let later = after(now, Duration::from_millis(1));
        let mut second = Round::opened_at(Epoch::new(1), B, voters.len(), later);
        for (voter, id) in voters.iter_mut().zip([ONE, TWO, THREE]) {
            let vote = voter.asked(&second.ballot(), later);
            assert_eq!(
                vote,
                Vote::Refused(Refused::EpochAlreadyDecided {
                    granted: Epoch::new(1)
                })
            );
            second.counts(id, vote);
        }
        assert_eq!(second.held(), None, "one epoch, one leader");
    }

    #[test]
    fn a_voter_grants_an_epoch_at_most_once_however_long_it_waits() {
        let now = base();
        let mut voter = settled(now);
        let ballot = Ballot {
            epoch: Epoch::new(7),
            candidate: A,
        };

        assert_eq!(voter.asked(&ballot, now), Vote::Granted);

        // Long after the grant it made has expired, so nothing but the epoch
        // rule itself can be doing the refusing.
        let long_after = after(now, LEASE_TTL.saturating_add(Duration::from_secs(60)));
        assert_eq!(
            voter.asked(&ballot, long_after),
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(7)
            })
        );
        assert_eq!(voter.decided(), Some(Epoch::new(7)));
    }

    #[test]
    fn a_voter_will_not_grant_again_while_the_grant_it_made_may_be_alive() {
        let now = base();
        let mut voter = settled(now);
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(1),
                    candidate: A
                },
                now
            ),
            Vote::Granted
        );

        let soon = after(now, Duration::from_secs(1));
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(2),
                    candidate: B
                },
                soon
            ),
            Vote::Refused(Refused::EarlierGrantStillAlive {
                for_the_next: LEASE_TTL.saturating_sub(Duration::from_secs(1))
            })
        );

        // Once its own hold has run out it is free, and it says so at exactly
        // the expiry rather than at the holder's fence — the guard is the gap
        // between those two and it belongs on this side.
        assert_eq!(voter.free_at(), Some(after(now, LEASE_TTL)));
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(2),
                    candidate: B
                },
                after(now, LEASE_TTL)
            ),
            Vote::Granted
        );
    }

    #[test]
    fn a_voter_that_has_just_started_sits_out_one_lease() {
        let started = base();
        let mut voter = Voter::started_at(started);
        let ballot = Ballot {
            epoch: Epoch::new(1),
            candidate: A,
        };

        let early = after(started, Duration::from_secs(4));
        assert_eq!(
            voter.asked(&ballot, early),
            Vote::Refused(Refused::TooSoonAfterStarting {
                for_the_next: LEASE_TTL.saturating_sub(Duration::from_secs(4))
            })
        );
        assert_eq!(voter.decided(), None, "a refusal decides nothing");

        assert_eq!(
            voter.asked(&ballot, after(started, LEASE_TTL)),
            Vote::Granted
        );
    }

    #[test]
    fn the_holder_stops_writing_before_the_earliest_voter_is_free_again() {
        let opened = base();
        let mut voters = [settled(opened), settled(opened), settled(opened)];
        let mut round = Round::opened_at(Epoch::new(3), A, voters.len(), opened);

        // A round that drags: one voter answers at once, one after a second,
        // one after three — comfortably longer than the guard, which is the
        // only shape in which dating the lease wrongly is detectable.
        let answered = [
            opened,
            after(opened, Duration::from_secs(1)),
            after(opened, Duration::from_secs(3)),
        ];
        let mut held = None;
        for ((voter, id), at) in voters.iter_mut().zip([ONE, TWO, THREE]).zip(answered) {
            let vote = voter.asked(&round.ballot(), at);
            assert_eq!(vote, Vote::Granted);
            held = round.counts(id, vote);
        }

        let held: Leadership = held.expect("three of three carried it");
        let earliest_free = voters
            .iter()
            .filter_map(Voter::free_at)
            .min()
            .expect("every voter granted");

        assert!(
            held.lease().fence() < earliest_free,
            "the holder must stop writing strictly before any voter may grant again"
        );
        assert_eq!(held.lease().expiry(), earliest_free);
        assert_eq!(
            held.lease().fence(),
            earliest_free
                .checked_sub(LEASE_GUARD)
                .expect("representable")
        );
    }

    #[test]
    fn a_round_that_dragged_past_the_window_hands_back_one_already_shut() {
        let opened = base();
        let mut voter = settled(opened);
        let mut round = Round::opened_at(Epoch::new(1), A, 1, opened);

        let answered = after(opened, LEASE_TTL.saturating_sub(Duration::from_secs(1)));
        let vote = voter.asked(&round.ballot(), answered);
        let held = round.counts(ONE, vote).expect("one of one carried it");

        assert!(
            held.lease().fenced(answered),
            "a lease dated from before the asking is already spent when the round was slower than the window"
        );
    }

    #[test]
    fn one_voter_answering_twice_does_not_carry_a_round() {
        let now = base();
        let mut round = Round::opened_at(Epoch::new(1), A, 3, now);
        assert_eq!(round.counts(ONE, Vote::Granted), None);
        assert_eq!(
            round.counts(ONE, Vote::Granted),
            None,
            "a majority is a majority of members, not of answers"
        );
        assert!(round.counts(TWO, Vote::Granted).is_some());
    }

    #[test]
    fn a_majority_is_strictly_more_than_half() {
        for (voters, needed) in [(1, 1), (2, 2), (3, 2), (4, 3), (5, 3), (6, 4), (7, 4)] {
            assert_eq!(majority(voters), needed, "majority of {voters}");
        }

        // The even case stated as the failure it prevents: two disjoint halves
        // of a set of four must not each carry a round.
        let now = base();
        let mut ours = Round::opened_at(Epoch::new(1), A, 4, now);
        ours.counts(ONE, Vote::Granted);
        assert_eq!(ours.counts(TWO, Vote::Granted), None, "half is not enough");
    }

    #[test]
    fn a_round_against_no_voters_can_never_conclude() {
        let now = base();
        let round = Round::opened_at(Epoch::new(1), A, 0, now);
        assert_eq!(round.held(), None);
    }
}
