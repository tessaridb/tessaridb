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

impl Voter {
    /// A voter that started now.
    #[must_use]
    pub fn started() -> Self {
        Self::started_at(Instant::now())
    }

    /// The epoch of the grant this voter holds, if it holds one.
    pub(crate) fn granted_epoch(&self) -> Option<Epoch> {
        self.granted.map(|granted| granted.epoch)
    }

    /// A voter that started at a stated instant.
    ///
    /// The instant is taken rather than read so that the restart rule can be
    /// exercised without a clock, and therefore without a test that can flake.
    #[must_use]
    pub const fn started_at(started: Instant) -> Self {
        Self {
            started,
            hold: LEASE_TTL,
            granted: None,
            seen: Epoch::ZERO,
        }
    }

    /// The same voter, holding every grant for `hold`.
    #[must_use]
    pub const fn holding_for(mut self, hold: Duration) -> Self {
        self.hold = hold;
        self
    }

    /// Hold every grant made from now on for `hold` — the policy the store has
    /// installed since. A grant already made keeps the hold it was made for.
    pub const fn hold_for(&mut self, hold: Duration) {
        self.hold = hold;
    }

    /// Answer one ballot.
    ///
    /// The checks are ordered by how different their remedies are. A log behind
    /// this voter's own is first because it is the only refusal that does not
    /// depend on what this voter has done — it is a statement about the
    /// candidate, true whatever this voter granted and whenever it started, and
    /// a candidate told to catch up has something to do about it. Then: a
    /// repeated epoch is a confused candidate; a live grant is the normal answer
    /// and the caller wants to know how long to wait; a recent start is a node
    /// sitting out a lease it cannot remember.
    ///
    /// # Both positions are arguments, and neither is read from a frame
    ///
    /// `mine` is this node's own log position and `candidate` is the position
    /// the candidate **proved** when it greeted — not one it wrote into the
    /// ballot. It is the rule [`crate::Error::NotItsOwnBallot`] already applies
    /// to the candidate's identity, for the same reason: a fact a candidate
    /// states about itself in the frame being judged is a fact it can choose,
    /// and an election restriction a candidate can opt out of restricts nothing.
    ///
    /// They are arguments rather than fields because a log position changes with
    /// every commit, so a voter that remembered one would be answering from a
    /// picture the store has already moved past.
    pub fn asked(
        &mut self,
        ballot: &Ballot,
        now: Instant,
        mine: Reached,
        candidate: Reached,
    ) -> Vote {
        // The incumbent renewing the epoch this voter's tail was written under
        // holds that tail by construction: one epoch has one leader, and it
        // wrote every entry under it. Judged on a greeting read before the
        // handshake, it would look behind the entries it streamed here since
        // (G057 SG6) — Raft never puts a leader's heartbeat to the election
        // restriction either.
        let renewing = self
            .granted
            .is_some_and(|held| held.candidate == ballot.candidate && held.epoch == ballot.epoch)
            && mine.leadership == ballot.epoch;
        if !renewing && candidate.behind(mine) {
            return Vote::Refused(Refused::LogBehind {
                leadership: mine.leadership,
                tail: mine.tail,
            });
        }
        // Judged against what was seen BEFORE this ballot, and adopted only
        // past the live-grant check below (Q-880, Raft's leader stickiness,
        // thesis §4.2.3). Adopting on every ballot let a challenger this voter
        // REFUSED for a live grant end that grant: the incumbent's renewal then
        // read as an epoch already decided, and a lease nobody had taken was
        // lost. Epoch order does not need it — a higher epoch that won was
        // granted by a majority, each member adopted it as it granted, and any
        // majority for a lower epoch includes one of them.
        let seen = self.seen;
        // Before the waiting refusals below, for the reason `LogBehind` is
        // first: a candidate told that the cluster has passed it has something
        // to do about it, where *wait six seconds* leaves it to stand again at
        // the same stale number.
        if ballot.epoch < seen {
            return Vote::Refused(Refused::EpochAlreadyDecided { granted: seen });
        }
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
            // The `<` half can no longer be reached — a granted epoch is a
            // seen epoch, so anything below it was refused above — and it stays
            // because `seen` is in-memory hygiene while this is the authority:
            // the day the two are made to disagree, the rule that matters is
            // still written where the grant is.
            if ballot.epoch < held.epoch || (ballot.epoch == held.epoch && !incumbent) {
                return Vote::Refused(Refused::EpochAlreadyDecided {
                    granted: held.epoch,
                });
            }
            let free = free_at(held.at, held.hold);
            if !incumbent && now < free {
                return Vote::Refused(Refused::EarlierGrantStillAlive {
                    for_the_next: free.saturating_duration_since(now),
                });
            }
        }
        // Past the live grant, the ballot is one this voter could grant, and its
        // epoch is adopted whatever comes next — including the restart refusal
        // below: a restarted voter that forgot a grant must still not go back to
        // granting below what it has been shown (G025 S3.2).
        self.seen = seen.max(ballot.epoch);
        if self.granted.is_none() {
            // Granted nothing since it started, so it cannot rule out having
            // granted something before it started.
            // The longer of the policy and the build: what this voter granted
            // before it restarted was held for the policy it ran under, which
            // the store still carries — and a policy shorter than the build's
            // must not shorten the window a build-length grant needs.
            let settled = free_at(self.started, self.hold.max(LEASE_TTL));
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
            hold: self.hold,
        });
        Vote::Granted { hold: self.hold }
    }

    /// Take a round a majority carried for `ballot`'s candidate — this node —
    /// into this voter's memory, whether or not this voter granted it; or
    /// refuse it, answering the higher epoch this voter has already granted.
    ///
    /// # A win this memory does not hold is a leader its own voter disowns
    ///
    /// The self-vote is cast last and can be refused: a voter that granted a
    /// rival an epoch moments ago refuses this node the next one for a whole
    /// TTL, and the peers carry the round without it. The win is sound — every
    /// voter in that majority kept its own promise, so the rival's lease cannot
    /// be alive — but the memory still names the rival. Read back as a grant to
    /// somebody else, it silenced the leader at its own standing gate for the
    /// rest of the lease, and once that grant aged out the same memory would
    /// grant the NEXT epoch to a challenger while this node still wrote under
    /// this one (G053 SG2d).
    ///
    /// A rival's grant of the SAME epoch is replaced too. Every voter grants an
    /// epoch once, so a majority that carried it for this node means the
    /// rival's bid at it lost; the record names a candidacy known to be over,
    /// and replacing it only ever refuses more — the rival is no longer the
    /// incumbent, and the hold runs from now.
    ///
    /// # A grant ABOVE the round ends the round
    ///
    /// A ballot this node's door granted while the round was in flight carried
    /// this node's log position as it then stood: the rival was promised it
    /// needed nothing after it. Leading the lower epoch would append past that
    /// promise — records acknowledged here and absent from the line a winning
    /// rival keeps. Raft's rule, for the same reason: a node that voted in a
    /// higher term is a follower in it, and an election for a lower term is
    /// over whatever its replies say. Decided under the one lock the door also
    /// takes, so no grant can land between the check and the record.
    ///
    /// # Errors
    ///
    /// The epoch this voter granted above the round, which the caller stands
    /// past next time.
    pub fn carried(&mut self, ballot: &Ballot, now: Instant) -> std::result::Result<(), Epoch> {
        if let Some(held) = self.granted
            && held.epoch > ballot.epoch
        {
            return Err(held.epoch);
        }
        self.seen = self.seen.max(ballot.epoch);
        self.granted = Some(Granted {
            epoch: ballot.epoch,
            candidate: ballot.candidate,
            at: now,
            hold: self.hold,
        });
        Ok(())
    }

    /// The highest epoch this voter has granted, if any.
    #[must_use]
    pub fn decided(&self) -> Option<Epoch> {
        self.granted.map(|held| held.epoch)
    }

    /// When this voter last granted a ballot **to a node that is not itself**,
    /// which is when it last had evidence that a leader was alive.
    ///
    /// # The `me` is the whole of it, and leaving it out cost a wave
    ///
    /// A node keeps ONE voting memory, shared by the peer door and by its own
    /// campaign, so that it cannot grant a single epoch twice. It follows that a
    /// candidate's self-vote lands here exactly like a peer's ballot — and until
    /// W275 this function reported it, so the standing gate read a node's own
    /// vote as proof that a leader was alive. The act of standing set the flag
    /// that forbids standing, for exactly `LEASE_TTL`: a leader was silenced for
    /// precisely as long as the lease it was trying to renew, and a lease could
    /// only be re-won after it had already been lost. Measured at W274 in a
    /// three-process run — two ten-second silences bracketing one round, one of
    /// them a sitting leader.
    ///
    /// The candidate was in the record the whole time and nothing read it. The
    /// argument for comparing it here rather than at the one call site is the
    /// one this tree applies everywhere else: a rule a caller has to remember is
    /// a rule that holds until the next caller.
    ///
    /// A node that voted for itself has heard nobody. That is not a refusal to
    /// answer — it is the answer.
    ///
    /// # It is the freshest liveness signal this node has, and it was already here
    ///
    /// A leader renews by putting a ballot to every voter at once, and it
    /// renews while two round times are left of its usable window — so a voter
    /// hears from a live leader roughly every `LEASE_TTL - LEASE_GUARD - 2 ×
    /// ROUND_MILLIS` plus one campaign tick, which is **about 300 ms** at
    /// today's values. The greeting directory that [`crate::heard_a_leader`]
    /// otherwise consults is refreshed on the awareness cadence, **a second**,
    /// and the reading itself is up to that old again. This instant is strictly
    /// fresher and costs nothing: the grant was already recorded, with `now` and
    /// not the earlier instant, precisely so that a renewal moves it.
    ///
    /// # A refusal is not evidence
    ///
    /// Only a grant is reported. Refusing a ballot says this voter would not
    /// have that node as leader — a candidate standing against a leader that is
    /// already gone refuses nothing and proves nothing about the leader. Reading
    /// a refusal as contact would let a dead cluster keep itself quiet by
    /// arguing with itself.
    #[must_use]
    pub fn granted_elsewhere_at(&self, me: [u8; NODE_ID_LEN]) -> Option<Instant> {
        self.granted
            .filter(|held| held.candidate != me)
            .map(|held| held.at)
    }

    /// The highest epoch this voter has been shown, granted or refused.
    #[must_use]
    pub const fn seen(&self) -> Epoch {
        self.seen
    }

    /// When this voter is next free to grant, if it has granted at all.
    ///
    /// The grantor's **expiry**, not the holder's fence: the guard is exactly the
    /// difference between the two, and it belongs on this side of the pair.
    #[must_use]
    pub fn free_at(&self) -> Option<Instant> {
        self.granted.map(|held| free_at(held.at, held.hold))
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
mod tests {
    use super::round::majority;
    use super::{Ballot, Deciding, Leadership, Reached, Refused, Round, Vote, Voter};
    use std::time::{Duration, Instant};
    use tessari_encoding::NODE_ID_LEN;
    use tessari_storage::{LEASE_GUARD, LEASE_TTL};

    /// `k` tenths of the lease. These cases were written in whole seconds
    /// against a ten-second lease; stated as fractions of it, they keep their
    /// meaning whatever the lease is (G053 SG2b).
    fn tenths(k: u32) -> Duration {
        LEASE_TTL
            .checked_div(10)
            .expect("a lease divides")
            .saturating_mul(k)
    }
    use tessari_types::{DatabaseId, Epoch, NamespaceId, Reach, Sequence, ShardId, TableId};

    /// A log position both sides of a vote share.
    ///
    /// Every case below is about the lease rules, so the two logs are level and
    /// the restriction added in W253 never fires — a test about when a voter may
    /// grant should not also be a test about what it is granting to.
    const LEVEL: Reached = Reached {
        leadership: Epoch::new(3),
        tail: Sequence::new(9),
    };

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
    const C: [u8; NODE_ID_LEN] = [0xC3; NODE_ID_LEN];
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
            let vote = voter.asked(&first.ballot(), now, LEVEL, LEVEL);
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
            let vote = voter.asked(&second.ballot(), later, LEVEL, LEVEL);
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
            range: tessari_types::Reach::Store,
        };

        assert_eq!(
            voter.asked(&ballot, now, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );

        let long_after = after(now, LEASE_TTL.saturating_add(Duration::from_secs(60)));
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(7),
                    candidate: B,
                    range: tessari_types::Reach::Store,
                },
                long_after,
                LEVEL,
                LEVEL
            ),
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(7)
            })
        );
        assert_eq!(
            voter.asked(&ballot, long_after, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL },
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
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            voter.asked(&ballot, now, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );

        // The last moment a renewal is any use: one instant before the holder
        // stops writing. The voter is still holding for `LEASE_GUARD` longer.
        let renewing = after(now, LEASE_TTL.saturating_sub(LEASE_GUARD));
        assert_eq!(
            voter.asked(&ballot, renewing, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL },
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
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            voter.asked(&ballot, now, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );

        let renewed = after(now, LEASE_TTL.saturating_sub(LEASE_GUARD));
        assert_eq!(
            voter.asked(&ballot, renewed, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
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
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                now,
                LEVEL,
                LEVEL
            ),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(4),
                    candidate: B,
                    range: tessari_types::Reach::Store,
                },
                after(now, tenths(1)),
                LEVEL,
                LEVEL
            ),
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(4)
            })
        );
    }

    #[test]
    fn a_grant_moves_the_instant_a_leader_is_judged_alive_by_and_a_refusal_does_not() {
        let now = base();
        let mut voter = settled(now);
        // Asked as C throughout: every grant below is to somebody else, which is
        // the case this test has always been about. The self-grant is the test
        // underneath this one.
        assert_eq!(
            voter.granted_elsewhere_at(C),
            None,
            "nothing granted, nothing to read"
        );

        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(1),
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                now,
                LEVEL,
                LEVEL
            ),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(voter.granted_elsewhere_at(C), Some(now));

        // A renewal from the incumbent moves it, because that is the whole
        // mechanism: a leader renews about every 300 ms and this is how a
        // voter knows the leader was alive that recently.
        let later = after(now, tenths(6));
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(1),
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                later,
                LEVEL,
                LEVEL
            ),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(voter.granted_elsewhere_at(C), Some(later));

        // A REFUSAL does not. B standing against the live incumbent is refused,
        // and refusing says this voter would not have B as leader — it is no
        // evidence at all that any leader is alive. Reading it as contact would
        // let a cluster whose leader is long gone keep itself quiet by arguing
        // with itself.
        let refused_at = after(later, tenths(1));
        assert!(matches!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(2),
                    candidate: B,
                    range: tessari_types::Reach::Store,
                },
                refused_at,
                LEVEL,
                LEVEL
            ),
            Vote::Refused(_)
        ));
        assert_eq!(
            voter.granted_elsewhere_at(C),
            Some(later),
            "a refusal moved the instant a leader is judged alive by"
        );
    }

    #[test]
    fn a_node_that_voted_for_itself_has_heard_nobody() {
        // The assertion W273 did not have, and the whole of Q-602. A candidate
        // self-votes through this same memory — one voting memory per node, so
        // that a node cannot grant one epoch twice — so its own ballot is
        // indistinguishable from a peer's unless the candidate is compared.
        //
        // Read the other way it would silence the only node that must not be
        // silenced: a leader renews by standing, standing self-votes, and a
        // self-vote read as a leader's liveness stops the next renewal for a
        // whole `LEASE_TTL` — exactly the lease being renewed.
        let now = base();
        let mut voter = settled(now);

        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(1),
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                now,
                LEVEL,
                LEVEL
            ),
            Vote::Granted { hold: LEASE_TTL }
        );

        assert_eq!(
            voter.granted_elsewhere_at(A),
            None,
            "a node read its own vote as evidence that a leader was alive"
        );
        // And the same grant, asked about by anybody else, still answers — the
        // filter is about who asked, not about forgetting the grant.
        assert_eq!(voter.granted_elsewhere_at(B), Some(now));
        assert_eq!(voter.granted_elsewhere_at(C), Some(now));
    }

    #[test]
    fn a_voter_will_not_grant_again_while_the_grant_it_made_may_be_alive() {
        let now = base();
        let mut voter = settled(now);
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(1),
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                now,
                LEVEL,
                LEVEL
            ),
            Vote::Granted { hold: LEASE_TTL }
        );

        let soon = after(now, tenths(1));
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(2),
                    candidate: B,
                    range: tessari_types::Reach::Store,
                },
                soon,
                LEVEL,
                LEVEL
            ),
            Vote::Refused(Refused::EarlierGrantStillAlive {
                for_the_next: LEASE_TTL.saturating_sub(tenths(1))
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
                    candidate: B,
                    range: tessari_types::Reach::Store,
                },
                after(now, LEASE_TTL),
                LEVEL,
                LEVEL
            ),
            Vote::Granted { hold: LEASE_TTL }
        );
    }

    #[test]
    fn a_voter_that_has_just_started_sits_out_one_lease() {
        let started = base();
        let mut voter = Voter::started_at(started);
        let ballot = Ballot {
            epoch: Epoch::new(1),
            candidate: A,
            range: tessari_types::Reach::Store,
        };

        let early = after(started, tenths(4));
        assert_eq!(
            voter.asked(&ballot, early, LEVEL, LEVEL),
            Vote::Refused(Refused::TooSoonAfterStarting {
                for_the_next: LEASE_TTL.saturating_sub(tenths(4))
            })
        );
        assert_eq!(voter.decided(), None, "a refusal decides nothing");

        assert_eq!(
            voter.asked(&ballot, after(started, LEASE_TTL), LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
    }

    #[test]
    fn a_voter_adopts_an_epoch_it_refused_and_will_not_grant_below_it_afterwards() {
        // G025 S3.2, and the hole it closes is entirely inside the restart
        // window. `a_voter_that_has_just_started_sits_out_one_lease` above
        // asserts the refusal; what it cannot see is that the refusal used to
        // throw the NUMBER away with the ballot.
        let started = base();
        let mut voter = Voter::started_at(started);

        let high = Ballot {
            epoch: Epoch::new(9),
            candidate: A,
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            voter.asked(&high, after(started, tenths(1)), LEVEL, LEVEL),
            Vote::Refused(Refused::TooSoonAfterStarting {
                for_the_next: LEASE_TTL.saturating_sub(tenths(1))
            })
        );
        assert_eq!(voter.decided(), None, "a refusal grants nothing");
        assert_eq!(
            voter.seen(),
            Epoch::new(9),
            "and it keeps the number regardless, because an epoch is the              cluster's count and not this voter's"
        );

        // One whole TTL later the start guard is spent and this voter may grant
        // again. Before the adoption it granted THIS — an epoch four below one
        // it had already been shown, to a candidate working from a stale
        // picture, with nothing in an error state.
        let low = Ballot {
            epoch: Epoch::new(5),
            candidate: B,
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            voter.asked(&low, after(started, LEASE_TTL), LEVEL, LEVEL),
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(9)
            })
        );
        assert_eq!(voter.decided(), None, "and still nothing has been granted");

        // The refusal is actionable rather than merely safe: it carries the
        // number, and a candidate that catches up to it wins.
        let caught_up = Ballot {
            epoch: Epoch::new(10),
            candidate: B,
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            voter.asked(&caught_up, after(started, LEASE_TTL), LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
    }

    #[test]
    fn an_epoch_refused_for_a_live_grant_does_not_end_that_grant() {
        // Q-880, Raft's leader stickiness (thesis §4.2.3). A voter holding a
        // live grant for A refuses a challenger at 9 — and until G053 SG2c it
        // adopted 9 as it refused, then refused A's own renewal at 3 as an
        // epoch already decided. A lease nobody had taken was lost, and a
        // leader-only acknowledgement with it (run 42, link 4).
        //
        // Epoch order does not need the adoption: a 9 that WON was granted by
        // a majority, every member of which adopted 9 as it granted, and any
        // majority for a lower epoch includes one of them.
        let opened = base();
        let mut voter = settled(opened);
        let ballot = |epoch: u64, candidate| Ballot {
            epoch: Epoch::new(epoch),
            candidate,
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            voter.asked(&ballot(3, A), opened, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(
            voter.asked(&ballot(9, B), opened, LEVEL, LEVEL),
            Vote::Refused(Refused::EarlierGrantStillAlive {
                for_the_next: LEASE_TTL
            }),
            "the hold it made for A is still alive, so 9 is refused"
        );
        assert_eq!(
            voter.asked(&ballot(3, A), after(opened, tenths(3)), LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL },
            "the refused challenger ended the incumbent's renewal"
        );
        assert_eq!(
            voter.seen(),
            Epoch::new(3),
            "a refused ballot raised the floor"
        );
    }

    #[test]
    fn the_holder_stops_writing_before_the_earliest_voter_is_free_again() {
        let opened = base();
        let mut voters = [settled(opened), settled(opened), settled(opened)];
        let mut round = Round::opened_at(Epoch::new(3), A, voters.len(), opened);

        // A round that drags: one voter answers at once, one after a tenth of
        // the lease, one after three tenths — longer than the guard, which is the
        // only shape in which dating the lease wrongly is detectable.
        let answered = [opened, after(opened, tenths(1)), after(opened, tenths(3))];
        let mut held = None;
        for ((voter, id), at) in voters.iter_mut().zip([ONE, TWO, THREE]).zip(answered) {
            let vote = voter.asked(&round.ballot(), at, LEVEL, LEVEL);
            assert_eq!(vote, Vote::Granted { hold: LEASE_TTL });
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

        let answered = after(opened, LEASE_TTL.saturating_sub(tenths(1)));
        let vote = voter.asked(&round.ballot(), answered, LEVEL, LEVEL);
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
        assert_eq!(round.counts(ONE, Vote::Granted { hold: LEASE_TTL }), None);
        assert_eq!(
            round.counts(ONE, Vote::Granted { hold: LEASE_TTL }),
            None,
            "a majority is a majority of members, not of answers"
        );
        assert!(
            round
                .counts(TWO, Vote::Granted { hold: LEASE_TTL })
                .is_some()
        );
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
        ours.counts(ONE, Vote::Granted { hold: LEASE_TTL });
        assert_eq!(
            ours.counts(TWO, Vote::Granted { hold: LEASE_TTL }),
            None,
            "half is not enough"
        );
    }

    #[test]
    fn a_round_against_no_voters_can_never_conclude() {
        let now = base();
        let round = Round::opened_at(Epoch::new(1), A, 0, now);
        assert_eq!(round.held(), None);
    }

    #[test]
    fn a_candidate_behind_this_voter_is_refused_and_told_how_far_to_come() {
        // ADR-0063's second half. Widening who may stand without this turns a
        // liveness improvement into a way to lose data: a candidate holding less
        // history wins, leads, and the writes it never received are gone with
        // nothing in an error state.
        let now = base();
        let mut voter = settled(now);
        let ballot = Ballot {
            epoch: Epoch::new(4),
            candidate: A,
            range: tessari_types::Reach::Store,
        };
        let behind = Reached {
            leadership: LEVEL.leadership,
            tail: Sequence::new(LEVEL.tail.get().saturating_sub(1)),
        };

        assert_eq!(
            voter.asked(&ballot, now, LEVEL, behind),
            Vote::Refused(Refused::LogBehind {
                leadership: LEVEL.leadership,
                tail: LEVEL.tail,
            }),
            "the refusal names the VOTER'S position, which is the half the \
             candidate does not already know"
        );
        // And the refusal is about the log rather than about this voter's state:
        // it granted nothing, so the same candidate level with it is granted.
        assert_eq!(voter.decided(), None);
        assert_eq!(
            voter.asked(&ballot, now, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
    }

    #[test]
    fn a_candidate_ahead_of_this_voter_is_not_refused_for_being_ahead() {
        // Strictly behind, not merely different. A voter that refused everyone
        // it was not level with would refuse every candidate in a cluster where
        // anything had been written since it last collected — which is every
        // cluster, most of the time.
        let now = base();
        let mut voter = settled(now);
        let ahead = Reached {
            leadership: LEVEL.leadership,
            tail: Sequence::new(LEVEL.tail.get().saturating_add(40)),
        };
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(4),
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                now,
                LEVEL,
                ahead
            ),
            Vote::Granted { hold: LEASE_TTL }
        );
    }

    #[test]
    fn a_longer_log_under_an_older_leadership_still_loses() {
        // The reason the comparison is a pair and not a number. A node that led
        // an epoch, wrote records no majority ever saw, and fell away holds a
        // HIGHER sequence than the node carrying the history that actually won.
        // Ranking on the sequence alone would hand leadership to the diverged
        // branch and call it the most up-to-date.
        let now = base();
        let mut voter = settled(now);
        let diverged = Reached {
            leadership: Epoch::new(LEVEL.leadership.get().saturating_sub(1)),
            tail: Sequence::new(LEVEL.tail.get().saturating_add(1_000)),
        };
        assert!(diverged.behind(LEVEL), "a lower leadership is behind");
        assert_eq!(
            voter.asked(
                &Ballot {
                    epoch: Epoch::new(9),
                    candidate: A,
                    range: tessari_types::Reach::Store,
                },
                now,
                LEVEL,
                diverged
            ),
            Vote::Refused(Refused::LogBehind {
                leadership: LEVEL.leadership,
                tail: LEVEL.tail,
            })
        );
        // And the other direction, which is what makes the pair an ordering
        // rather than a preference: a shorter log under a newer leadership wins.
        assert!(!LEVEL.behind(diverged));
    }

    #[test]
    fn the_leader_renewing_its_own_epoch_is_never_behind_what_it_wrote() {
        // G057 SG6. The candidate's position comes from the greeting it proved,
        // read before a handshake of several round trips; this voter's is read
        // when the ballot lands. A leader committing all the while has streamed
        // this voter entries past the greeting, so across distance the voter
        // looked ahead of the very leader whose epoch wrote its tail — and
        // refused the renewal until the lease ran out under writes. Everything
        // written under one epoch was written by its one leader, so its renewal
        // holds it by construction; an election is still judged as before.
        let now = base();
        let mut voter = settled(now);
        let epoch = Epoch::new(7);
        let renewal = Ballot {
            epoch,
            candidate: A,
            range: tessari_types::Reach::Store,
        };
        let greeted = Reached {
            leadership: epoch,
            tail: Sequence::new(2),
        };
        assert_eq!(
            voter.asked(&renewal, now, greeted, greeted),
            Vote::Granted { hold: LEASE_TTL }
        );
        let streamed = Reached {
            leadership: epoch,
            tail: Sequence::new(41),
        };
        let later = after(now, tenths(3));
        assert_eq!(
            voter.asked(&renewal, later, streamed, greeted),
            Vote::Granted { hold: LEASE_TTL },
            "the incumbent's renewal of its own epoch"
        );
        // Control: a challenger with the same stale position is still behind.
        let mut other = settled(now);
        assert_eq!(
            other.asked(
                &Ballot {
                    epoch: Epoch::new(8),
                    candidate: B,
                    range: tessari_types::Reach::Store,
                },
                now,
                streamed,
                greeted
            ),
            Vote::Refused(Refused::LogBehind {
                leadership: epoch,
                tail: Sequence::new(41),
            })
        );
    }

    #[test]
    fn two_logs_at_one_position_are_behind_neither() {
        // The self-vote depends on this: a candidate asks its own memory with
        // its own position on both sides, and a rule that refused equality would
        // stop every node voting for itself.
        assert!(!LEVEL.behind(LEVEL));
    }

    #[test]
    fn a_refusal_that_names_a_log_crosses_the_wire() {
        // The reason a refusal carries values at all: *catch up to sequence 9*
        // and *you are re-running a decided epoch* send a candidate to different
        // places, and a wire that kept only the "no" would be less informative
        // than the rule behind it.
        let refused = Vote::Refused(Refused::LogBehind {
            leadership: Epoch::new(6),
            tail: Sequence::new(4_096),
        });
        assert_eq!(
            Vote::decode(&refused.encode()).expect("a vote this build wrote"),
            refused
        );
    }

    // ---- G032 S3.1 and S3.2: a ballot names its line -------------------------

    fn shard(n: u32) -> Reach {
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            ShardId::new(n),
        )
    }

    #[test]
    fn a_store_ballot_keeps_its_twenty_four_bytes() {
        // The kill criterion (G032), from `Ballot::encode` as it stood at
        // `a1d0025`: the epoch big-endian, then the candidate.
        let ballot = Ballot {
            epoch: Epoch::new(7),
            candidate: [5; NODE_ID_LEN],
            range: Reach::Store,
        };
        let mut golden = vec![0, 0, 0, 0, 0, 0, 0, 7];
        golden.extend_from_slice(&[5; NODE_ID_LEN]);
        assert_eq!(ballot.encode(), golden);
        assert_eq!(Ballot::decode(&golden).expect("a store ballot"), ballot);
    }

    #[test]
    fn a_range_ballot_round_trips_and_a_cut_range_is_refused() {
        let ballot = Round::opened(Epoch::new(3), [6; NODE_ID_LEN], 3)
            .over(shard(2))
            .ballot();
        assert_eq!(ballot.range, shard(2));
        let body = ballot.encode();
        assert_eq!(Ballot::decode(&body).expect("a range ballot"), ballot);
        for stop in 25..body.len() {
            let cut = body.get(..stop).expect("a prefix");
            assert!(
                Ballot::decode(cut).is_err(),
                "{stop} bytes read as a ballot"
            );
        }
    }

    fn settled_deciding() -> Deciding {
        Deciding::holding(Voter::started_at(
            base()
                .checked_sub(LEASE_TTL)
                .expect("an hour ahead minus ten seconds"),
        ))
    }

    fn ballot(epoch: u64, candidate: u8, range: Reach) -> Ballot {
        Ballot {
            epoch: Epoch::new(epoch),
            candidate: [candidate; NODE_ID_LEN],
            range,
        }
    }

    #[test]
    fn a_grant_on_one_line_never_answers_a_ballot_on_another() {
        let deciding = settled_deciding();
        let now = base();
        let vote = |ballot: &Ballot| deciding.asked(ballot, now, LEVEL, LEVEL);
        assert_eq!(
            vote(&ballot(1, 1, shard(1))),
            Vote::Granted { hold: LEASE_TTL }
        );
        // The same line and epoch for somebody else: one epoch, one candidate.
        assert!(matches!(
            vote(&ballot(1, 2, shard(1))),
            Vote::Refused(Refused::EpochAlreadyDecided { .. })
        ));
        // Another line's epoch 1 is another counter, and so is the store's.
        assert_eq!(
            vote(&ballot(1, 2, shard(2))),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(
            vote(&ballot(1, 2, Reach::Store)),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(
            deciding.granted_elsewhere_on(shard(1), [2; NODE_ID_LEN]),
            Some(now)
        );
        assert_eq!(
            deciding.granted_elsewhere_on(shard(1), [1; NODE_ID_LEN]),
            None
        );
        assert_eq!(
            deciding.granted_elsewhere_on(shard(3), [2; NODE_ID_LEN]),
            None
        );
    }

    #[test]
    fn a_grant_to_a_new_store_epoch_is_announced_and_a_renewal_is_not() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let deciding = settled_deciding();
        let announced = Arc::new(AtomicUsize::new(0));
        let counting = Arc::clone(&announced);
        assert!(deciding.when_granted_anew(Box::new(move || {
            counting.fetch_add(1, Ordering::Relaxed);
        })));
        let now = base();
        let candidate = [1; NODE_ID_LEN];

        assert_eq!(
            deciding.asked(&store_ballot(1, candidate), now, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(announced.load(Ordering::Relaxed), 1);
        // The incumbent renewing its epoch is the same leadership.
        assert_eq!(
            deciding.asked(&store_ballot(1, candidate), now, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(announced.load(Ordering::Relaxed), 1);
        // A placed range's line is not the store's leadership.
        let _ = deciding.asked(&ballot(1, 2, shard(1)), now, LEVEL, LEVEL);
        assert_eq!(announced.load(Ordering::Relaxed), 1);
        // Free again, a later epoch is a new leadership.
        let later = now
            .checked_add(LEASE_TTL.saturating_mul(2))
            .expect("in range");
        assert_eq!(
            deciding.asked(&store_ballot(2, candidate), later, LEVEL, LEVEL),
            Vote::Granted { hold: LEASE_TTL }
        );
        assert_eq!(announced.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn every_line_starts_when_the_process_did() {
        // A restarted voter cannot remember a grant on ANY line, so a line it
        // has never been asked about is as young as the process.
        let started = base();
        let deciding = Deciding::holding(Voter::started_at(started));
        let vote = deciding.asked(&ballot(1, 1, shard(1)), started, LEVEL, LEVEL);
        assert!(
            matches!(vote, Vote::Refused(Refused::TooSoonAfterStarting { .. })),
            "{vote:?}"
        );
    }

    /// A ballot for `candidate` at `epoch` on the store's line.
    fn store_ballot(epoch: u64, candidate: [u8; NODE_ID_LEN]) -> Ballot {
        Ballot {
            epoch: Epoch::new(epoch),
            candidate,
            range: Reach::Store,
        }
    }

    #[test]
    fn a_voter_holds_its_grant_for_the_lease_its_policy_states() {
        // G053 SG2c (Q-878). A policy that lengthens the lease lengthens what a
        // voter promises, or a holder writing under the long lease would meet a
        // voter that had already freed itself on the build's short one.
        let now = base();
        let hold = LEASE_TTL.saturating_mul(4);
        let mut voter =
            Voter::started_at(now.checked_sub(hold).expect("representable")).holding_for(hold);
        assert_eq!(
            voter.asked(&store_ballot(1, A), now, LEVEL, LEVEL),
            Vote::Granted { hold },
            "a grant states how long its voter will hold it"
        );
        let past_the_built_in_lease = after(now, LEASE_TTL.saturating_add(tenths(1)));
        let vote = voter.asked(&store_ballot(2, B), past_the_built_in_lease, LEVEL, LEVEL);
        assert!(
            matches!(vote, Vote::Refused(Refused::EarlierGrantStillAlive { .. })),
            "a voter freed itself on the build's lease while its policy's still ran: {vote:?}"
        );
        assert_eq!(voter.free_at(), Some(after(now, hold)));
    }

    #[test]
    fn a_restarted_voter_sits_out_the_longer_of_its_policy_and_the_build() {
        // A restarted voter cannot remember what it granted, and what it granted
        // was held for the policy it ran under — which the store still carries.
        let started = base();
        let hold = LEASE_TTL.saturating_mul(4);
        let mut voter = Voter::started_at(started).holding_for(hold);
        let vote = voter.asked(
            &store_ballot(1, A),
            after(started, LEASE_TTL.saturating_add(tenths(1))),
            LEVEL,
            LEVEL,
        );
        assert!(
            matches!(vote, Vote::Refused(Refused::TooSoonAfterStarting { .. })),
            "{vote:?}"
        );
        let mut short = Voter::started_at(started).holding_for(tenths(2));
        let vote = short.asked(&store_ballot(1, A), after(started, tenths(5)), LEVEL, LEVEL);
        assert!(
            matches!(vote, Vote::Refused(Refused::TooSoonAfterStarting { .. })),
            "a short policy shortened the restart window below the build's lease: {vote:?}"
        );
    }

    #[test]
    fn a_lease_is_no_longer_than_the_shortest_hold_that_carried_it() {
        // The holder stops before ANY voter that granted it is free, whichever
        // policy each had installed when it answered — and before its own.
        let opened = base();
        let mut round =
            Round::opened_at(Epoch::new(3), A, 3, opened).leasing(LEASE_TTL.saturating_mul(4));
        assert_eq!(
            round.counts(
                ONE,
                Vote::Granted {
                    hold: LEASE_TTL.saturating_mul(4)
                }
            ),
            None
        );
        let held = round
            .counts(
                TWO,
                Vote::Granted {
                    hold: LEASE_TTL.saturating_mul(2),
                },
            )
            .expect("two of three");
        assert_eq!(
            held.lease().expiry(),
            after(opened, LEASE_TTL.saturating_mul(2)),
            "the lease outlived a voter's hold"
        );

        let mut modest = Round::opened_at(Epoch::new(4), A, 1, opened).leasing(LEASE_TTL);
        let held = modest
            .counts(
                ONE,
                Vote::Granted {
                    hold: LEASE_TTL.saturating_mul(4),
                },
            )
            .expect("one of one");
        assert_eq!(
            held.lease().expiry(),
            after(opened, LEASE_TTL),
            "a voter's longer hold lengthened the candidate's own lease"
        );
    }

    #[test]
    fn a_grant_states_its_hold_on_the_wire_and_a_bare_one_is_the_builds() {
        let hold = Duration::from_millis(3_250);
        let granted = Vote::Granted { hold };
        assert_eq!(Vote::decode(&granted.encode()).ok(), Some(granted));
        // What a build from before the field sends: the tag alone. It held its
        // grant for its own lease, and the shortest this build assumes is its own.
        assert_eq!(
            Vote::decode(&[0]).ok(),
            Some(Vote::Granted { hold: LEASE_TTL })
        );
    }
}
