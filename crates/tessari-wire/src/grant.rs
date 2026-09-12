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

use crate::error::{Error, Result};
use crate::frame;

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

impl Ballot {
    /// The body of a [`crate::PeerFrame::Ballot`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(24);
        frame::put_u64(&mut body, self.epoch.get());
        body.extend_from_slice(&self.candidate);
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape a ballot
    /// takes.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (epoch, at) = frame::take_u64(body, 0)?;
        let mut candidate = [0_u8; NODE_ID_LEN];
        let rest = body
            .get(at..at.saturating_add(NODE_ID_LEN))
            .ok_or(Error::Malformed)?;
        candidate.copy_from_slice(rest);
        Ok(Self {
            epoch: Epoch::new(epoch),
            candidate,
        })
    }
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

impl Vote {
    /// The body of a [`crate::PeerFrame::Vote`] frame.
    ///
    /// A refusal keeps its reason and its duration across the wire. *Wait six
    /// seconds* and *you are re-running a decided epoch* send a candidate to
    /// different places, and a wire that collapsed them into "no" would be less
    /// informative than the rules behind it.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(9);
        match self {
            Self::Granted => body.push(0),
            Self::Refused(Refused::EpochAlreadyDecided { granted }) => {
                body.push(1);
                frame::put_u64(&mut body, granted.get());
            }
            Self::Refused(Refused::EarlierGrantStillAlive { for_the_next }) => {
                body.push(2);
                frame::put_u64(&mut body, millis(*for_the_next));
            }
            Self::Refused(Refused::TooSoonAfterStarting { for_the_next }) => {
                body.push(3);
                frame::put_u64(&mut body, millis(*for_the_next));
            }
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is empty or short, and
    /// [`Error::UnknownFrame`] when it names an answer this build does not
    /// have — which is a newer peer rather than a broken one, and the tag it
    /// carries is the answer it gave.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let kind = *body.first().ok_or(Error::Malformed)?;
        match kind {
            0 => Ok(Self::Granted),
            1 => {
                let (granted, _) = frame::take_u64(body, 1)?;
                Ok(Self::Refused(Refused::EpochAlreadyDecided {
                    granted: Epoch::new(granted),
                }))
            }
            2 => {
                let (left, _) = frame::take_u64(body, 1)?;
                Ok(Self::Refused(Refused::EarlierGrantStillAlive {
                    for_the_next: Duration::from_millis(left),
                }))
            }
            3 => {
                let (left, _) = frame::take_u64(body, 1)?;
                Ok(Self::Refused(Refused::TooSoonAfterStarting {
                    for_the_next: Duration::from_millis(left),
                }))
            }
            tag => Err(Error::UnknownFrame { tag }),
        }
    }
}

/// A duration as whole milliseconds, saturating.
///
/// The wire carries no more precision than an operator can act on, and a value
/// too large to represent is reported as the largest one rather than wrapped
/// into a small wait.
fn millis(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

/// One voting member's memory of what it has granted.
///
/// Deliberately tiny: the highest epoch it granted and when. Those two facts are
/// the entire state a voter needs, which is why the restart rule can substitute
/// for persisting them.
#[derive(Debug)]
pub struct Voter {
    started: Instant,
    granted: Option<Granted>,
}

/// The one grant a voter is holding, and who it is holding it for.
///
/// The candidate is here because re-granting to the node that **already holds
/// it** produces one holder, and one holder is the whole of what
/// [`Refused::EarlierGrantStillAlive`] protects. A voter that remembered only
/// the epoch and the instant could not tell an incumbent renewing from a
/// challenger arriving, so it refused both — which made every lease terminal,
/// since a leader must renew strictly before its own fence shuts and the fence
/// shuts `LEASE_GUARD` before the voter is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Granted {
    epoch: Epoch,
    candidate: [u8; NODE_ID_LEN],
    at: Instant,
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
        if let Some(held) = self.granted {
            // The node this voter is already holding a grant for. Both rules
            // below turn on it, and neither is safe without the **proved**
            // identity behind it — `Link::greet` refuses a ballot naming anyone
            // but the peer that presented the credential, because from here a
            // claimed name is indistinguishable from a true one.
            let incumbent = ballot.candidate == held.candidate;
            // A renewal re-asks its own epoch, so `<=` would refuse every one of
            // them. The invariant safety actually needs is *one epoch, one
            // CANDIDATE* — `two_candidates_cannot_both_carry_one_epoch` — and
            // that is what this says. An epoch below the one held is somebody
            // working from a stale picture whoever they are.
            if ballot.epoch < held.epoch || (ballot.epoch == held.epoch && !incumbent) {
                return Vote::Refused(Refused::EpochAlreadyDecided {
                    granted: held.epoch,
                });
            }
            let free = free_at(held.at);
            if !incumbent && now < free {
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

        // `now` and not the earlier grant's instant, including on a renewal:
        // the voter's own hold runs from the grant it most recently made, or it
        // would come free while the lease it had just extended was still alive.
        self.granted = Some(Granted {
            epoch: ballot.epoch,
            candidate: ballot.candidate,
            at: now,
        });
        Vote::Granted
    }

    /// The highest epoch this voter has granted, if any.
    #[must_use]
    pub fn decided(&self) -> Option<Epoch> {
        self.granted.map(|held| held.epoch)
    }

    /// When this voter is next free to grant, if it has granted at all.
    ///
    /// The grantor's **expiry**, not the holder's fence: the guard is exactly the
    /// difference between the two, and it belongs on this side of the pair.
    #[must_use]
    pub fn free_at(&self) -> Option<Instant> {
        self.granted.map(|held| free_at(held.at))
    }
}

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
    pub fn asked(&self, ballot: &Ballot, now: Instant) -> Vote {
        self.held().asked(ballot, now)
    }

    /// The highest epoch this node has granted, if any.
    #[must_use]
    pub fn decided(&self) -> Option<Epoch> {
        self.held().decided()
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Voter> {
        self.voter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    fn a_voter_grants_one_epoch_to_one_candidate_however_long_it_waits() {
        // **Narrowed in W228, deliberately, and this comment is the record.**
        // This asserted *an epoch at most once*, which is stronger than the
        // property it was protecting: what carries the safety is one epoch, one
        // CANDIDATE — `two_candidates_cannot_both_carry_one_epoch`, untouched.
        // The stronger reading also made renewal impossible, because a renewal
        // re-asks its own epoch (only an election advances one, since the log's
        // divergence check reads an epoch as a leadership generation).
        //
        // So the refusal is asserted against a DIFFERENT candidate, and the same
        // one is asserted to be granted. Both long after the first grant has
        // expired, so nothing but the epoch rule itself can be doing either.
        let now = base();
        let mut voter = settled(now);
        let ballot = Ballot {
            epoch: Epoch::new(7),
            candidate: A,
        };

        assert_eq!(voter.asked(&ballot, now), Vote::Granted);

        let long_after = after(now, LEASE_TTL.saturating_add(Duration::from_secs(60)));
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(7),
                    candidate: B
                },
                long_after
            ),
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(7)
            })
        );
        assert_eq!(
            voter.asked(&ballot, long_after),
            Vote::Granted,
            "the holder re-asking its own epoch adds no second holder"
        );
        assert_eq!(voter.decided(), Some(Epoch::new(7)));
    }

    #[test]
    fn an_incumbent_may_renew_before_the_lease_it_holds_expires() {
        // The hole this wave exists to close. A leader has to renew strictly
        // before its own fence shuts, which is `LEASE_GUARD` before the lease
        // expires — and the voter's hold runs to the expiry itself, so every
        // renewal that is not already too late arrives inside a window the
        // voter is still holding.
        //
        // Re-granting to the node that already holds it produces one holder,
        // which is the whole of the property `EarlierGrantStillAlive` protects.
        let now = base();
        let mut voter = settled(now);
        let ballot = Ballot {
            epoch: Epoch::new(4),
            candidate: A,
        };
        assert_eq!(voter.asked(&ballot, now), Vote::Granted);

        // The last moment a renewal is any use: one instant before the holder
        // stops writing. The voter is still holding for `LEASE_GUARD` longer.
        let renewing = after(now, LEASE_TTL.saturating_sub(LEASE_GUARD));
        assert_eq!(
            voter.asked(&ballot, renewing),
            Vote::Granted,
            "a leader that cannot renew before its own fence holds a terminal lease"
        );
    }

    #[test]
    fn a_renewal_moves_the_window_the_next_challenger_waits_out() {
        // A renewal is a grant, so the voter's own hold is measured from it. A
        // renewal that refreshed the holder without refreshing the voter would
        // free the voter while the lease it had just extended was alive, which
        // is the split-brain the guard exists to prevent, arriving by the one
        // door this wave opens.
        let now = base();
        let mut voter = settled(now);
        let ballot = Ballot {
            epoch: Epoch::new(4),
            candidate: A,
        };
        assert_eq!(voter.asked(&ballot, now), Vote::Granted);

        let renewed = after(now, LEASE_TTL.saturating_sub(LEASE_GUARD));
        assert_eq!(voter.asked(&ballot, renewed), Vote::Granted);
        assert_eq!(
            voter.free_at(),
            Some(after(renewed, LEASE_TTL)),
            "the voter is free one TTL after the renewal, not after the first grant"
        );
    }

    #[test]
    fn a_challenger_cannot_take_the_epoch_its_holder_is_still_renewing() {
        // The other half, and the reason the candidate has to be compared rather
        // than the epoch alone: B asking for A's live epoch is the impersonation
        // case with the name left off.
        let now = base();
        let mut voter = settled(now);
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(4),
                    candidate: A
                },
                now
            ),
            Vote::Granted
        );
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(4),
                    candidate: B
                },
                after(now, Duration::from_secs(1))
            ),
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(4)
            })
        );
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
