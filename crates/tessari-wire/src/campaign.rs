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

use rustls::pki_types::CertificateDer;

use tessari_encoding::NODE_ID_LEN;
use tessari_storage::Lease;
use tessari_types::{Epoch, Reach};

use crate::grant::{Deciding, Leadership, Refused, Round, Vote};
use crate::link::{Answered, Ask, Credential, call};
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
    /// What this node shows a peer, and the key proving it is ours.
    pub mine: &'a Credential,
    /// The authority every peer's credential must chain to.
    pub authority: &'a CertificateDer<'a>,
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
    pub fn renew(&self, voter: &Deciding, held: Lease, next: Epoch, now: Instant) -> Stood {
        if renew_in(held, self.round, now) > Duration::ZERO {
            return Stood::NotDue;
        }
        let mut round = Round::opened_at(
            next,
            self.candidate,
            self.peers.len().saturating_add(1),
            now,
        )
        .over(self.range);
        let ballot = round.ballot();
        // Both sides of the comparison are this node's own greeting, so the log
        // restriction never refuses a candidate its own vote — a node is not
        // behind itself. It is passed rather than skipped because the rule lives
        // in one place, and a self-vote that took a different path through it
        // would be a second rule nobody is reading.
        let mut granted = Epoch::ZERO;
        for (peer, address) in self.peers {
            // One short of carried, not carried: the ballot below is this
            // node's own and costs no handshake, so a peer asked past this
            // point would have its voter spent — for a whole `LEASE_TTL` — on a
            // round that was already decided. The old order asked the same
            // number of peers by counting the self-vote first.
            if round.needs() <= 1 {
                break;
            }
            let Ok((_, answered)) = call(
                *address,
                self.mine.duplicate(),
                self.authority,
                *peer,
                self.said,
                Ask::Ballot(&round.ballot()),
            ) else {
                continue;
            };
            let Answered::Voted(vote) = answered else {
                continue;
            };
            note(&mut granted, vote);
            round.counts(*peer, vote);
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
        round.held().map_or(Stood::Lost { granted }, Stood::Won)
    }
}

/// Keep the highest epoch a refusal reported having been granted.
///
/// This node's own vote is passed through it too, and deliberately: a candidate
/// whose own voting memory has moved past the epoch it is standing for has
/// learned the same fact from the same kind of answer, and reading it twice in
/// two places is how the two come to disagree.
fn note(highest: &mut Epoch, vote: Vote) {
    if let Vote::Refused(Refused::EpochAlreadyDecided { granted }) = vote {
        if granted > *highest {
            *highest = granted;
        }
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
mod tests {
    use super::{Standing, Stood};
    use crate::grant::{Ballot, Deciding, Refused, Vote, Voter};
    use crate::link::tests::{Authority, THERE, hello, settled, voting};
    use crate::link::{Answered, Ask, Credential, Peers, call};
    use crate::peer::{Hello, Purpose};
    use rustls::pki_types::CertificateDer;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};
    use tessari_encoding::NODE_ID_LEN;
    use tessari_storage::{LEASE_GUARD, LEASE_TTL, Lease};
    use tessari_types::Epoch;

    /// A round that takes a tenth of a second — long enough that two of them is
    /// a margin a test can state, short enough that every case here runs at
    /// once.
    const ROUND: Duration = Duration::from_millis(100);

    /// A log position both sides of a vote share — see the note on the constant
    /// of the same name in `grant`.
    const LEVEL: crate::grant::Reached = crate::grant::Reached {
        leadership: Epoch::new(3),
        tail: tessari_types::Sequence::new(9),
    };

    fn standing<'a>(
        mine: &'a Credential,
        authority: &'a CertificateDer<'a>,
        said: &'a Hello,
        peers: &'a [([u8; NODE_ID_LEN], SocketAddr)],
    ) -> Standing<'a> {
        Standing {
            candidate: THERE,
            mine,
            authority,
            said,
            peers,
            round: ROUND,
            range: tessari_types::Reach::Store,
        }
    }

    /// The candidate's own voting memory, settled as of `now`.
    ///
    /// A node votes for itself through the same memory a peer's ballot reaches,
    /// so it is subject to the same rules — including the restart rule, which is
    /// why a freshly started one refuses the candidate's own ballot and is used
    /// deliberately in the test that asserts exactly that.
    ///
    /// It takes `now` rather than reading the clock, and the first draft did
    /// read the clock: `settled()` starts a voter one TTL before **its own**
    /// call, so a helper evaluated as an argument started fractionally after the
    /// `now` the round was opened at, and the voter refused every self-vote on
    /// the restart rule. Four tests failed at once and none of them was about
    /// restarts. The instant a test states is the instant everything in it has
    /// to be measured against.
    fn mine_voting(now: Instant) -> Deciding {
        Deciding::holding(Voter::started_at(
            now.checked_sub(LEASE_TTL.saturating_mul(2))
                .expect("this machine has been up for twenty seconds"),
        ))
    }

    /// A lease with exactly `margin` of writable time left as of `now`.
    fn leaving(margin: Duration, now: Instant) -> Lease {
        Lease::taken_at(now, LEASE_GUARD.checked_add(margin).expect("representable"))
    }

    #[test]
    fn a_lost_round_reports_the_highest_epoch_a_voter_said_it_had_granted() {
        // ADR-0066's learning half, against a real refusal over the wire rather
        // than a constructed one. The peer has already granted epoch 40 to
        // somebody else, so it refuses this ballot and says so — and that number
        // is what stops a candidate climbing one epoch per round towards a
        // cluster it is far behind.
        let authority = Authority::new();
        let voter = [64_u8; NODE_ID_LEN];
        let now = Instant::now();
        let mut spent = Voter::started_at(
            now.checked_sub(LEASE_TTL.saturating_mul(2))
                .expect("this machine has been up for twenty seconds"),
        );
        let elsewhere = Ballot {
            epoch: Epoch::new(40),
            candidate: [200_u8; NODE_ID_LEN],
            range: tessari_types::Reach::Store,
        };
        assert_eq!(
            spent.asked(&elsewhere, now, LEVEL, LEVEL),
            Vote::Granted,
            "the voter has to have granted 40 for the refusal below to name it"
        );
        let (address, answering) = voting(&authority, voter, spent);

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let peers = [(voter, address)];
        let standing = standing(&mine, &der, &said, &peers);

        assert_eq!(
            standing.renew(
                &mine_voting(now),
                leaving(Duration::ZERO, now),
                Epoch::new(2),
                now
            ),
            Stood::Lost {
                granted: Epoch::new(40)
            },
            "the round threw away the one number the refusal was carrying"
        );
        drop(answering.join().expect("the door's thread"));
    }

    #[test]
    fn a_leader_with_margin_left_asks_nobody() {
        // The door would grant — that is what makes this a test of the ordering
        // rather than of the arithmetic. If the canvass ran and its answer was
        // discarded, the epoch would be spent at the voter all the same, and a
        // node that did this on every tick would exhaust the willingness of the
        // very members it needs when its fence finally approaches.
        let authority = Authority::new();
        let voter = [50_u8; NODE_ID_LEN];
        let (address, answering) = voting(&authority, voter, settled());

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let peers = [(voter, address)];
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        let held = leaving(Duration::from_secs(5), now);
        assert_eq!(
            standing.renew(&mine_voting(now), held, Epoch::new(2), now),
            Stood::NotDue,
            "five seconds of margin against a tenth-second round"
        );

        // The proof that nobody was asked is the voter's own memory: the epoch
        // a canvass would have burnt is still there to be granted.
        let (_, vote) = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            voter,
            &hello(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(2),
                candidate: THERE,
                range: tessari_types::Reach::Store,
            }),
        )
        .expect("the door is still up, having been asked nothing");
        assert_eq!(
            vote,
            Answered::Voted(Vote::Granted),
            "the epoch was never spent, so it is still grantable"
        );
        drop(answering.join().expect("the door's thread"));
    }

    #[test]
    fn a_leader_stands_early_enough_to_lose_a_round_and_still_renew() {
        // A round and a half of margin. One round time still fits, so a cadence
        // that subtracted only one would sit still here and stand at the last
        // moment that can possibly work — leaving nothing for a round that is
        // refused, lost or slow. Two round times is what makes the retry
        // reachable, and this is the case that tells them apart.
        let authority = Authority::new();
        let voter = [51_u8; NODE_ID_LEN];
        let (address, answering) = voting(&authority, voter, settled());

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let peers = [(voter, address)];
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        let held = leaving(
            ROUND
                .checked_add(Duration::from_millis(50))
                .expect("representable"),
            now,
        );
        let won = standing
            .renew(&mine_voting(now), held, Epoch::new(2), now)
            .won()
            .expect("inside two round times, a leader stands");
        assert_eq!(won.epoch, Epoch::new(2));
        drop(answering.join().expect("the door's thread"));
    }

    #[test]
    fn a_leader_at_its_fence_wins_the_next_epoch() {
        let authority = Authority::new();
        let voters = [
            [52_u8; NODE_ID_LEN],
            [53_u8; NODE_ID_LEN],
            [54_u8; NODE_ID_LEN],
        ];
        let mut doors: Vec<_> = voters
            .into_iter()
            .map(|id| (id, voting(&authority, id, settled())))
            .collect();
        let peers: Vec<_> = doors
            .iter()
            .map(|(id, (address, _))| (*id, *address))
            .collect();

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        let won = standing
            .renew(
                &mine_voting(now),
                leaving(Duration::ZERO, now),
                Epoch::new(7),
                now,
            )
            .won()
            .expect("a majority of four — this node and two of its three peers");
        let answered = Instant::now();

        assert_eq!(won.epoch, Epoch::new(7));
        // Dated from when the round opened, not from when the majority came
        // back. The gap between the two is real — three TLS handshakes happened
        // in it — and it is charged to this node's own window rather than to the
        // voters', which is the rule the seam exists to carry.
        assert_eq!(won.from, now, "dated from the instant the round opened");
        assert!(answered > now, "and the collection delay was not nothing");

        // The membership is four — three peers and this node — so a majority is
        // three: this node's own vote and two peers'. The round therefore ended
        // at the second door and the third was never asked. That is not an
        // accident of iteration order — it is the round concluding the moment it
        // is carried — and it shows in the third voter's untouched memory: the
        // epoch is still grantable, which it would not be had the ballot reached
        // it.
        let (spare, (address, untouched)) = doors.pop().expect("three doors");
        for (_, (_, door)) in doors {
            drop(door.join().expect("the door's thread"));
        }
        let (_, vote) = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            spare,
            &hello(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(7),
                candidate: THERE,
                range: tessari_types::Reach::Store,
            }),
        )
        .expect("the third door is still up, having been asked nothing");
        assert_eq!(
            vote,
            Answered::Voted(Vote::Granted),
            "the third voter never saw the ballot the first two carried"
        );
        drop(untouched.join().expect("the door's thread"));
    }

    #[test]
    fn a_peer_that_is_gone_does_not_cost_the_round_its_majority() {
        let authority = Authority::new();
        let gone = [55_u8; NODE_ID_LEN];
        // A real address with nothing behind it: the door is opened to learn
        // where it would have been, then closed. Placed FIRST in the peer set,
        // because a canvass that aborted on the error would still reach a
        // majority if the unreachable member came last.
        let absent = {
            let door = Peers::bind(
                "127.0.0.1:0",
                authority.issue(gone, Purpose::Peer),
                &authority.der(),
            )
            .expect("a door on loopback");
            door.address().expect("its address")
        };

        let voters = [[56_u8; NODE_ID_LEN], [57_u8; NODE_ID_LEN]];
        let doors: Vec<_> = voters
            .into_iter()
            .map(|id| (id, voting(&authority, id, settled())))
            .collect();
        let mut peers = vec![(gone, absent)];
        peers.extend(doors.iter().map(|(id, (address, _))| (*id, *address)));

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        let won = standing
            .renew(
                &mine_voting(now),
                leaving(Duration::ZERO, now),
                Epoch::new(3),
                now,
            )
            .won()
            .expect("this node and the two that answered carry a membership of four");
        assert_eq!(won.epoch, Epoch::new(3));

        for (_, (_, door)) in doors {
            drop(door.join().expect("the door's thread"));
        }
    }
    #[test]
    fn a_cluster_of_three_carries_a_round_with_one_member_down() {
        // The arithmetic this wave exists for. The membership is three — this
        // node and the two peers an operator declared — so a majority is two:
        // this node's own vote and one peer's. Counting only the peers would
        // make it two OF TWO, and a cluster of three that cannot survive a
        // single loss has no majority in the sense a majority is for. That is
        // not an inefficiency, it is the failover in S7.1 being impossible by
        // arithmetic rather than by any missing mechanism.
        let authority = Authority::new();

        // A real address with nothing behind it, placed FIRST so a round that
        // gave up on the error would not reach the live member either.
        let gone = [60_u8; NODE_ID_LEN];
        let absent = {
            let door = Peers::bind(
                "127.0.0.1:0",
                authority.issue(gone, Purpose::Peer),
                &authority.der(),
            )
            .expect("a door on loopback");
            door.address().expect("its address")
        };

        let alive = [61_u8; NODE_ID_LEN];
        let (address, answering) = voting(&authority, alive, settled());

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let peers = [(gone, absent), (alive, address)];
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        let won = standing
            .renew(
                &mine_voting(now),
                leaving(Duration::ZERO, now),
                Epoch::new(9),
                now,
            )
            .won()
            .expect("this node and the one peer that answered are two of three");
        assert_eq!(won.epoch, Epoch::new(9));
        drop(answering.join().expect("the door's thread"));
    }

    #[test]
    fn a_node_that_voted_for_itself_refuses_that_epoch_to_a_rival() {
        // The reason the self-vote goes through the node's own memory rather
        // than being added to a tally. A vote counted but not recorded would
        // leave this node free to grant the same epoch to somebody else moments
        // later — two candidates holding one epoch, each with an honest
        // majority, and nothing anywhere in an error state.
        let authority = Authority::new();
        let voter = [62_u8; NODE_ID_LEN];
        let (address, answering) = voting(&authority, voter, settled());

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let peers = [(voter, address)];
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        let ours = mine_voting(now);
        let won = standing
            .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(11), now)
            .won()
            .expect("this node and its one peer are two of two");
        assert_eq!(won.epoch, Epoch::new(11));

        // Asked exactly as a peer would ask, on the connection the door serves.
        let rival = [63_u8; NODE_ID_LEN];
        let vote = ours.asked(
            &Ballot {
                epoch: Epoch::new(11),
                candidate: rival,
                range: tessari_types::Reach::Store,
            },
            Instant::now(),
            LEVEL,
            LEVEL,
        );
        assert!(
            matches!(
                vote,
                Vote::Refused(Refused::EpochAlreadyDecided { granted }) if granted == Epoch::new(11)
            ),
            "a node granted one epoch to two candidates: {vote:?}"
        );
        assert_eq!(
            ours.decided(),
            Some(Epoch::new(11)),
            "the node's own ballot left no trace in its own memory"
        );
        drop(answering.join().expect("the door's thread"));
    }

    #[test]
    fn a_node_that_will_not_vote_for_itself_does_not_count_itself() {
        // A voter that has just started cannot rule out having granted something
        // it has forgotten, so it sits out one TTL. That rule is about the NODE,
        // which means it applies to a ballot the node wrote as surely as to one
        // that arrived on a socket — and a candidate that exempted itself from
        // it would be spending the exact safety the restart rule buys.
        //
        // One peer, so the membership is two and a majority is both. The peer
        // grants; this node refuses itself; the round is not carried.
        let authority = Authority::new();
        let voter = [64_u8; NODE_ID_LEN];
        let (address, answering) = voting(&authority, voter, settled());

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let peers = [(voter, address)];
        let standing = standing(&mine, &der, &said, &peers);

        let now = Instant::now();
        assert_eq!(
            standing.renew(
                &Deciding::started(),
                leaving(Duration::ZERO, now),
                Epoch::new(13),
                now
            ),
            Stood::Lost {
                granted: Epoch::ZERO
            },
            "a node just restarted counted a vote it had refused to cast"
        );
        drop(answering.join().expect("the door's thread"));
    }
}
