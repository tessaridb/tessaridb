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
//! **No campaign.** Nothing here decides *when* to stand and nothing renews on a
//! timer; that is policy over these rules rather than a change to them.
//!
//! # One grant per epoch is not enough on its own
//!
//! It stops two leaders holding one epoch. It says nothing about *which* of
//! several eligible candidates should hold it, and while exactly one node could
//! stand that gap cost nothing — the only candidate's log was the cluster's by
//! definition. ADR-0063 widened who may stand, and the gap became a way to lose
//! data silently: a candidate holding less history wins a majority, leads, and
//! the writes it never received are gone with nothing anywhere in an error
//! state.
//!
//! So a voter also refuses a candidate whose log is behind its own, which is
//! Raft's election restriction. The comparison is ADR-0059's ordering over
//! [`Reached`] — a higher leadership first, the higher sequence at equal
//! leadership — and the refusal names the voter's own position, so a candidate
//! can tell *catch up* from *you lost*.

mod deciding;
mod round;
mod voter;
use std::time::{Duration, Instant};

use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{LEASE_TTL, Lease};
use tessari_types::{Epoch, Reach, Sequence};

use crate::error::{Error, Result};
use crate::frame;
pub use deciding::Deciding;
pub use round::Round;
pub(crate) use round::free_at;

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
    /// Which election line the epoch is on (ADR-0082).
    ///
    /// [`Reach::Store`] for the store line, which is every ballot a build before
    /// placement ever put; another range for a placed range's own line. Epochs
    /// of two lines are unrelated counters, so a voter keeps one memory per line
    /// and a grant on one never answers a ballot on another.
    pub range: Reach,
}

impl Ballot {
    /// The body of a [`crate::PeerFrame::Ballot`] frame.
    ///
    /// The range is a tail written only when it is not the store, so a store
    /// ballot keeps the twenty-four bytes it has always had and an older voter
    /// reads it unchanged.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(24);
        frame::put_u64(&mut body, self.epoch.get());
        body.extend_from_slice(&self.candidate);
        if self.range != Reach::Store {
            frame::put_reach(&mut body, self.range);
        }
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
        // A body that ends here is a store ballot, from this build or an older
        // one; anything after it is the range and must read whole.
        let at = at.saturating_add(NODE_ID_LEN);
        let range = if body.len() > at {
            frame::take_reach(body, at)?.0
        } else {
            Reach::Store
        };
        Ok(Self {
            epoch: Epoch::new(epoch),
            candidate,
            range,
        })
    }
}

/// How far a log has got, as the pair that orders two of them.
///
/// The sequence alone does not order two logs. A node that led an epoch, wrote
/// records no majority ever saw, and fell away can hold a **higher** sequence
/// than the node carrying the history that actually won — so ranking on the
/// number would promote the diverged branch. The leadership that wrote the tail
/// is what breaks that tie, and it is the fact ADR-0059 put in every record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reached {
    /// The leadership under which the record at `tail` was written.
    pub leadership: Epoch,
    /// How far the committed log reaches.
    pub tail: Sequence,
}

impl Reached {
    /// Whether this log is strictly behind `other`.
    ///
    /// ADR-0059's ordering, written out rather than derived: a higher leadership
    /// wins outright, and the higher sequence decides only within one
    /// leadership. Deriving it from field order would make a safety rule a
    /// property of how the struct happens to be declared.
    ///
    /// Strictly, so that two logs at the same position are not behind each
    /// other — a voter must be able to grant to a candidate level with it, and a
    /// candidate must be able to vote for itself.
    #[must_use]
    pub fn behind(self, other: Self) -> bool {
        match self.leadership.cmp(&other.leadership) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => self.tail < other.tail,
        }
    }
}

/// Why a voter said no.
///
/// Four refusals rather than one, because they send an operator somewhere
/// different: an epoch already decided means a candidate is re-running a round
/// that concluded; a grant still alive is the healthy steady state; a voter too
/// recently started is a node that has restarted and is deliberately sitting out
/// one lease; and a log behind this voter's own is a candidate that must not
/// lead yet whatever else is true.
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
    /// The candidate's log is behind this voter's own.
    ///
    /// It carries the voter's own position and not the candidate's, which the
    /// candidate already knows. The pair is what makes the answer actionable:
    /// the same leadership and a higher sequence says *catch up and stand
    /// again*, a higher leadership says *the history you hold is not the one
    /// that won*, and those send whoever reads them to different places.
    LogBehind {
        /// The leadership under which this voter's own tail was written.
        leadership: Epoch,
        /// How far this voter's own committed log reaches.
        tail: Sequence,
    },
    /// The ballot is for a placed range, and this voter's catalog does not
    /// place the candidate on it (ADR-0098).
    ///
    /// What makes a placement move: once a voter has applied the change, the
    /// node it took the range from can no longer renew its lease there.
    NotPlaced,
}

/// A voting member's answer to one ballot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    /// This voter will not grant this epoch to anyone else, for `hold`.
    Granted {
        /// How long this voter holds the grant: the lease its installed
        /// failover policy states (G053 SG2c). A holder's lease is the
        /// shortest hold among the grants that carried it, so a policy
        /// installed on one node before another can never leave the holder
        /// writing after a voter has freed itself.
        hold: Duration,
    },
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
        let mut body = Vec::with_capacity(17);
        match self {
            Self::Granted { hold } => {
                body.push(0);
                frame::put_u64(&mut body, millis(*hold));
            }
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
            Self::Refused(Refused::LogBehind { leadership, tail }) => {
                body.push(4);
                frame::put_u64(&mut body, leadership.get());
                frame::put_u64(&mut body, tail.get());
            }
            Self::Refused(Refused::NotPlaced) => body.push(5),
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
            // A build from before the hold sends the tag alone. It held its
            // grant for its own lease, and the shortest this build will assume
            // is its own — the holder's lease only ever comes out shorter.
            0 if body.len() == 1 => Ok(Self::Granted { hold: LEASE_TTL }),
            0 => {
                let (hold, _) = frame::take_u64(body, 1)?;
                Ok(Self::Granted {
                    hold: Duration::from_millis(hold),
                })
            }
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
            4 => {
                let (leadership, at) = frame::take_u64(body, 1)?;
                let (tail, _) = frame::take_u64(body, at)?;
                Ok(Self::Refused(Refused::LogBehind {
                    leadership: Epoch::new(leadership),
                    tail: Sequence::new(tail),
                }))
            }
            5 => Ok(Self::Refused(Refused::NotPlaced)),
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
    /// How long a grant made now is held: the installed failover policy's
    /// lease, or the build's when nobody set one (G053 SG2c).
    hold: Duration,
    granted: Option<Granted>,
    /// The highest epoch this voter has been shown, granted or not.
    ///
    /// Granting is the only thing [`Granted`] records, and the gap that leaves
    /// is a whole restart window wide: a ballot for epoch 9 arriving inside
    /// [`Refused::TooSoonAfterStarting`] is refused and **forgotten**, so one
    /// `LEASE_TTL` later a ballot for epoch 5 finds a voter that remembers
    /// nothing and grants it — below an epoch the cluster had already reached,
    /// with nothing anywhere in an error state.
    ///
    /// So the number is adopted from every ballot this voter sees, which is
    /// Raft §5.1's rule for `currentTerm` — adopt first, judge afterwards —
    /// minus the persistence, because the start guard already covers the window
    /// persistence would be protecting.
    seen: Epoch,
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
    /// The hold this grant was made for. Kept with the grant rather than read
    /// from the voter, so a policy installed afterwards neither shortens a
    /// promise already made nor lengthens one nobody was told about.
    hold: Duration,
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
    /// How long the lease runs from `from`: the shortest of the candidate's own
    /// policy lease and every granting voter's hold (G053 SG2c).
    pub length: Duration,
}

impl Leadership {
    /// The lease this grant entitles the holder to.
    ///
    /// Dated from [`Leadership::from`], so a round that took a long time hands its
    /// holder a shorter window rather than one that outlives the voters'.
    #[must_use]
    pub fn lease(&self) -> Lease {
        Lease::taken_at(self.from, self.length)
    }
}

#[cfg(test)]
mod tests;
