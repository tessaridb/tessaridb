//! When a leader stands again, and what it puts to its peers when it does.
//!
//! # The decision is taken before the network, not after
//!
//! [`Standing::renew`] answers `None` **without opening a socket** while the
//! holder still has margin. That ordering is the whole of this module's safety
//! argument, and it is not an optimisation: a voter grants an epoch at most
//! once, so a candidate that canvassed on every tick and threw the answer away
//! would burn one epoch per tick at every voter it reached. The cheapest way to
//! exhaust a cluster's willingness to elect anybody is to ask it constantly.
//!
//! # Two round times, because one is the deadline rather than the margin
//!
//! A round opened at `t` and answered at `t + r` yields a lease dated `t` — see
//! [`crate::Leadership`] for why the collection delay comes out of the winner's
//! window and never out of the voters'. So to keep writing, the new lease has
//! to be in hand before the old fence shuts: `r < left(t)`.
//!
//! That makes **one** round time the hard deadline. Standing there lands exactly
//! on the fence with no room for a round that is lost, refused or slow. **Two**
//! is the latest opening that still allows one complete retry, which is derived
//! from the dating rule rather than picked because it looked cautious.
//!
//! `round` is a parameter and not a constant, because how long a round takes is
//! a property of the network this cluster is on and not of this engine.
//!
//! # What is not here
//!
//! **No thread, no timer, no daemon.** This module decides *whether* to stand
//! and runs the canvass if the answer is yes; deciding *when* to ask belongs
//! with the node's own lifecycle. A driver built here would have to own a clock,
//! and a component that owns a clock cannot be tested the way everything else in
//! this workspace is — by stating the instant and asserting the consequence.
//!
//! **No follower loop.** Nothing pulls a replica forward on a timer. Same
//! missing shape, different criterion.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tessari_encoding::NODE_ID_LEN;
use tessari_storage::Lease;
use tessari_types::{Epoch, Reach};

use crate::grant::{Ballot, Deciding, Leadership, Refused, Round, Vote};
use crate::keys::PeerKeys;
use crate::link::{Answered, Ask, call_within};
use crate::peer::Hello;

/// What one pass at standing actually did.
///
/// `Option<Leadership>` said *won* or *not won*, and those are three answers
/// wearing two names: a round that never opened because the margin was intact,
/// and a round that opened and lost, are the same value and want opposite
/// treatment. The second has to move the candidate on; the first must not touch
/// it (ADR-0066).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stood {
    /// The margin had not been spent, so nobody was asked.
    NotDue,
    /// A majority granted the epoch.
    Won(Leadership),
    /// The round opened and no majority granted it.
    Lost {
        /// The highest epoch any voter reported having already granted, and
        /// [`Epoch::ZERO`] when none of them said.
        ///
        /// It is the number a candidate needs to stop climbing one epoch per
        /// round towards a conversation that is already far above it — a node
        /// that never led holds `Epoch::ZERO` whatever the cluster has reached,
        /// and every refusal it receives is already carrying the answer.
        granted: Epoch,
    },
}

/// Everything a node holds in order to be able to stand for an epoch.
///
/// It is a struct rather than six more arguments on a free function because the
/// call underneath already takes six of its own, and a nine-parameter signature
/// is refused by the linter and by the next reader at about the same point.
#[derive(Debug)]
pub struct Standing<'a> {
    /// The id this node stands under — the candidate on every ballot it puts.
    pub candidate: [u8; NODE_ID_LEN],
    /// What this node shows a peer, whom it trusts and whom it refuses.
    pub keys: &'a PeerKeys,
    /// The greeting that opens each connection.
    pub said: &'a Hello,
    /// The voting members, and where to reach each one.
    ///
    /// Addresses are already resolved: a cluster's membership is configuration
    /// rather than something to look up again under the pressure of a closing
    /// fence.
    pub peers: &'a [([u8; NODE_ID_LEN], SocketAddr)],
    /// How long a round takes on this network, end to end.
    pub round: Duration,
    /// Which election line this standing is for — [`Reach::Store`] for the
    /// store's, a placed range for its own (ADR-0082).
    pub range: Reach,
    /// The lease this node's installed failover policy states — the longest a
    /// round it wins may hand it, before any voter's shorter hold cuts it down
    /// (G053 SG2c).
    pub lease: Duration,
}

impl Stood {
    /// The leadership this pass won, if it won one.
    ///
    /// For a caller that only needs to know whether it leads. The three-way
    /// match is for the one caller that has to act differently on a loss —
    /// [`crate::Renewing`], which is where the memory of a lost round lives.
    #[must_use]
    pub fn won(self) -> Option<Leadership> {
        match self {
            Self::Won(leadership) => Some(leadership),
            Self::NotDue | Self::Lost { .. } => None,
        }
    }
}

impl Standing<'_> {
    /// Stand for `next` if `held` is close enough to its fence to need it.
    ///
    /// Answers `None` in two different circumstances that are worth telling
    /// apart at the call site only by what the caller does next: there was
    /// margin left and nobody was asked, or a round was run and no majority
    /// granted it. Both mean *carry on with the lease you have*, and the second
    /// is the ordinary outcome of standing against a healthy incumbent.
    ///
    /// A peer that cannot be reached, or that answers something other than a
    /// vote, is skipped rather than fatal — a round is carried by a majority of
    /// the members, not by all of them, and one node being down is the condition
    /// this whole mechanism exists to survive.
    ///
    /// # The voting set is the peers **and this node**
    ///
    /// `peers` is what an operator declared, and a node never appears in its own
    /// peer list — it cannot pick itself out of one, which is why a routing
    /// target is carried as a field rather than derived by matching. So the
    /// membership is `peers.len() + 1`, and counting only the dialable part of
    /// it gets the arithmetic backwards in the worst available direction: in a
    /// cluster of three it would make `majority` two **of two**, so a round
    /// would need every surviving peer and the loss of any single member would
    /// end leadership permanently. A majority that cannot survive one failure is
    /// not what a majority is for.
    ///
    /// # This node's own vote goes through its own memory
    ///
    /// The candidate asks [`Deciding`] first, with exactly the ballot a peer
    /// receives. A self-vote counted *without* being recorded would leave this
    /// node free to grant the same epoch to somebody else moments later — two
    /// candidates, one epoch, each with an honest majority, and nothing anywhere
    /// in an error state.
    ///
    /// It follows that a node can refuse to vote for itself, and that is the
    /// rules working rather than a case to special-case: a voter restarted less
    /// than one TTL ago cannot rule out having granted something it has
    /// forgotten, and that is as true of a ballot it wrote as of one that
    /// arrived on a socket.
    #[must_use]
    pub async fn renew(&self, voter: &Deciding, held: Lease, next: Epoch, now: Instant) -> Stood {
        if renew_in(held, self.round, now) > Duration::ZERO {
            return Stood::NotDue;
        }
        let mut round = Round::opened_at(
            next,
            self.candidate,
            self.peers.len().saturating_add(1),
            now,
        )
        .over(self.range)
        .leasing(self.lease);
        let ballot = round.ballot();
        let opened = Instant::now();
        // Both sides of the comparison are this node's own greeting, so the log
        // restriction never refuses a candidate its own vote — a node is not
        // behind itself. It is passed rather than skipped because the rule lives
        // in one place, and a self-vote that took a different path through it
        // would be a second rule nobody is reading.
        let mut granted = Epoch::ZERO;
        for (peer, vote) in self.canvass(ballot).await {
            note(&mut granted, vote);
            round.counts(peer, vote);
        }
        // The candidate's own ballot is cast LAST, and only when it still
        // decides something. Casting it first is what W257 found had to change:
        // a grant is a LEASE, so a voter that made one refuses everybody else
        // until that lease is certainly dead — a whole `LEASE_TTL`. A candidate
        // that voted for itself on every round therefore spent its own voter on
        // a round it had already lost, and three candidates doing that starve
        // each other of voters permanently: epochs climb, every answer is
        // `EarlierGrantStillAlive`, and nobody is ever elected.
        //
        // It is still cast when the peers alone carried the round, and that is
        // not an optimisation to remove. A winner whose own voter holds no
        // record of the grant would go on to grant the NEXT epoch to somebody
        // else while it was itself still writing under this one.
        if round.needs() <= 1 {
            let reached = self.said.reached();
            let mine = voter.asked(&ballot, now, reached, reached);
            note(&mut granted, mine);
            round.counts(self.candidate, mine);
        }
        // Carried by a majority is carried, whether or not this node's own
        // voter was in it — and its memory has to say so, or the winner reads
        // its own leadership as a grant to somebody else. Unless that memory
        // granted a HIGHER epoch while the round was in flight: then the round
        // is over, and the next one stands past it (`Voter::carried`).
        let stood = round.held().map_or(Stood::Lost { granted }, |leadership| {
            match voter.carried(&ballot, now) {
                Ok(()) => Stood::Won(leadership),
                Err(promised) => Stood::Lost {
                    granted: granted.max(promised),
                },
            }
        });
        ran(self.range, held, now, opened, stood);
        stood
    }
}

/// How a round went against the fence it was defending: how long the canvass
/// took, how much of the held lease was left when it opened, and how old the
/// instant it was decided on already was (Q-946); off unless asked for.
fn ran(range: Reach, held: Lease, now: Instant, opened: Instant, stood: Stood) {
    let micros = |took: Duration| u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        range = ?range,
        elapsed_us = micros(opened.elapsed()),
        left_us = micros(held.left(opened)),
        stale_us = micros(opened.saturating_duration_since(now)),
        won = matches!(stood, Stood::Won(_)),
        "a renewal round ran"
    );
}

impl Standing<'_> {
    /// Put `ballot` to every voting peer at once, each dial bounded by the round.
    ///
    /// # Every peer, and not only until a majority is reached
    ///
    /// A renewal is also the only evidence a voter has that its leader is alive
    /// ([`crate::Voter::granted_elsewhere_at`]), and once the lease is under a
    /// second the greeting directory is too old to testify. A canvass that
    /// stopped at a majority left one voter in three hearing nothing: it stood,
    /// its ballot raised every voter's epoch past the incumbent's, and the next
    /// renewal was refused (G053 SG2b). A peer granting the winner a round that
    /// was already carried spends nothing it should keep — its grant is to the
    /// node that is leading, which is exactly what it refuses challengers for.
    ///
    /// # At once, on the runtime, and bounded by the round
    ///
    /// One after another, a member that accepted the connection and then said
    /// nothing held the canvass for the greeting's ten seconds, past every lease
    /// this build ships. Each ballot is a task on the caller's runtime — on its
    /// blocking pool, because the peer link is synchronous (`link.rs`), the same
    /// shape the peer door serves connections in — and none may take longer than
    /// the round on any one step: a blocking task cannot be cancelled, so its
    /// deadline is the only thing that ends it. A peer that cannot be reached,
    /// answers something other than a vote, or whose task failed, are all one
    /// answer: no vote.
    async fn canvass(&self, ballot: Ballot) -> Vec<([u8; NODE_ID_LEN], Vote)> {
        let mut asked = tokio::task::JoinSet::new();
        for (peer, address) in self.peers {
            let (peer, address, said, round) = (*peer, *address, *self.said, self.round);
            let keys = self.keys.clone();
            asked.spawn_blocking(move || {
                match call_within(
                    address,
                    (&keys, keys.duplicate()),
                    peer,
                    &said,
                    Ask::Ballot(&ballot),
                    round,
                ) {
                    Ok((_, Answered::Voted(vote))) => Some((peer, vote)),
                    _ => None,
                }
            });
        }
        let mut votes = Vec::with_capacity(self.peers.len());
        while let Some(answered) = asked.join_next().await {
            if let Ok(Some(vote)) = answered {
                votes.push(vote);
            }
        }
        votes
    }
}

/// Keep the highest epoch a refusal reported having been granted.
///
/// This node's own vote is passed through it too, and deliberately: a candidate
/// whose own voting memory has moved past the epoch it is standing for has
/// learned the same fact from the same kind of answer, and reading it twice in
/// two places is how the two come to disagree.
fn note(highest: &mut Epoch, vote: Vote) {
    if let Vote::Refused(Refused::EpochAlreadyDecided { granted }) = vote
        && granted > *highest
    {
        *highest = granted;
    }
}

/// How long a holder may wait before it has to stand again.
///
/// Zero means *now*: the margin is spent and the next round has to start, while
/// there is still room for it to be lost once and run again. See the module
/// header for why the subtrahend is two round times and not one.
fn renew_in(held: Lease, round: Duration, now: Instant) -> Duration {
    held.left(now).saturating_sub(round.saturating_mul(2))
}

#[cfg(test)]
mod tests;
