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

use crate::grant::{Ballot, Deciding, Leadership, Refused, Round, Vote};
use crate::link::{Answered, Ask, Credential, call_within};
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
        .over(self.range);
        let ballot = round.ballot();
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
        round.held().map_or(Stood::Lost { granted }, |leadership| {
            match voter.carried(&ballot, now) {
                Ok(()) => Stood::Won(leadership),
                Err(promised) => Stood::Lost {
                    granted: granted.max(promised),
                },
            }
        })
    }
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
            let mine = self.mine.duplicate();
            let authority = self.authority.clone().into_owned();
            asked.spawn_blocking(move || {
                match call_within(
                    address,
                    (mine, &authority),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_round_reports_the_highest_epoch_a_voter_said_it_had_granted() {
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
            standing
                .renew(
                    &mine_voting(now),
                    leaving(Duration::ZERO, now),
                    Epoch::new(2),
                    now
                )
                .await,
            Stood::Lost {
                granted: Epoch::new(40)
            },
            "the round threw away the one number the refusal was carrying"
        );
        drop(answering.join().expect("the door's thread"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leader_with_margin_left_asks_nobody() {
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
            standing
                .renew(&mine_voting(now), held, Epoch::new(2), now)
                .await,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leader_stands_early_enough_to_lose_a_round_and_still_renew() {
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
            .await
            .won()
            .expect("inside two round times, a leader stands");
        assert_eq!(won.epoch, Epoch::new(2));
        drop(answering.join().expect("the door's thread"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leader_at_its_fence_wins_the_next_epoch() {
        let authority = Authority::new();
        let voters = [
            [52_u8; NODE_ID_LEN],
            [53_u8; NODE_ID_LEN],
            [54_u8; NODE_ID_LEN],
        ];
        let doors: Vec<_> = voters
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
            .await
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
        // three. Until G053 SG2b the round ended at the second door and the
        // third was never asked; now every door is asked at once, because a
        // renewal is the only evidence a voter has that its leader lives. Each
        // door answers exactly one connection, so every one of them having
        // finished is the proof that every one of them was asked.
        for (_, (_, door)) in doors {
            drop(door.join().expect("the door's thread"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_is_gone_does_not_cost_the_round_its_majority() {
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
            .await
            .won()
            .expect("this node and the two that answered carry a membership of four");
        assert_eq!(won.epoch, Epoch::new(3));

        for (_, (_, door)) in doors {
            drop(door.join().expect("the door's thread"));
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cluster_of_three_carries_a_round_with_one_member_down() {
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
            .await
            .won()
            .expect("this node and the one peer that answered are two of three");
        assert_eq!(won.epoch, Epoch::new(9));
        drop(answering.join().expect("the door's thread"));
    }

    /// A voting door whose memory the test keeps, so it can read what the door
    /// granted after the round — and a way to release it if it was never asked.
    fn kept_voting(
        authority: &Authority,
        id: [u8; NODE_ID_LEN],
    ) -> (
        SocketAddr,
        std::sync::Arc<Deciding>,
        std::thread::JoinHandle<()>,
    ) {
        let door = Peers::bind(
            "127.0.0.1:0",
            authority.issue(id, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = door.address().expect("the door's address");
        let deciding = std::sync::Arc::new(Deciding::holding(settled()));
        let held = std::sync::Arc::clone(&deciding);
        let answering = std::thread::spawn(move || {
            drop(door.greet(|| Ok(hello(id)), &id, &held, &crate::collection::NoLog));
        });
        (address, deciding, answering)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewal_reaches_every_voter_and_not_only_a_majority() {
        // G053 SG2b. A voter that hears no renewal has no evidence its leader is
        // alive once the lease is under a second, so it stands — and its ballot
        // raises every voter's epoch past the incumbent's. A canvass that
        // stopped at a majority left one voter in three hearing nothing at all.
        let authority = Authority::new();
        let ids = [
            [70_u8; NODE_ID_LEN],
            [71_u8; NODE_ID_LEN],
            [72_u8; NODE_ID_LEN],
        ];
        let doors: Vec<_> = ids
            .iter()
            .map(|id| (*id, kept_voting(&authority, *id)))
            .collect();
        let peers: Vec<_> = doors
            .iter()
            .map(|(id, (address, _, _))| (*id, *address))
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
                Epoch::new(5),
                now,
            )
            .await
            .won()
            .expect("four members, all granting");
        assert_eq!(won.epoch, Epoch::new(5));

        for (id, (address, deciding, answering)) in doors {
            let decided = deciding.decided();
            if decided.is_none() {
                // Never asked, so its door is still waiting: knock to release it.
                drop(std::net::TcpStream::connect(address));
            }
            answering.join().expect("the door's thread");
            assert_eq!(
                decided,
                Some(Epoch::new(5)),
                "voter {} heard no renewal, so it has no evidence its leader lives",
                id[0]
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_never_answers_costs_the_round_at_most_its_deadline() {
        // A member whose host accepts the connection and then says nothing —
        // a hung process, a half-open link. The dial used the greeting's ten
        // seconds, so one such peer placed first held the whole canvass past
        // every lease this build ships (G053 SG2b).
        let authority = Authority::new();
        let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let quiet = silent.local_addr().expect("its address");
        let holding = std::thread::spawn(move || silent.accept().map(|(socket, _)| socket));

        let alive = [81_u8; NODE_ID_LEN];
        let (address, deciding, answering) = kept_voting(&authority, alive);
        let peers = [([80_u8; NODE_ID_LEN], quiet), (alive, address)];

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let standing = standing(&mine, &der, &said, &peers);
        let now = Instant::now();
        let won = standing
            .renew(
                &mine_voting(now),
                leaving(Duration::ZERO, now),
                Epoch::new(6),
                now,
            )
            .await
            .won();
        let took = now.elapsed();
        drop(holding.join());
        answering.join().expect("the door's thread");

        assert!(
            took < ROUND.saturating_mul(5),
            "a silent member held the round for {took:?} against a {ROUND:?} deadline"
        );
        assert_eq!(won.map(|held| held.epoch), Some(Epoch::new(6)));
        assert_eq!(deciding.decided(), Some(Epoch::new(6)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_voted_for_itself_refuses_that_epoch_to_a_rival() {
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
            .await
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_will_not_vote_for_itself_does_not_count_itself() {
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
            standing
                .renew(
                    &Deciding::started(),
                    leaving(Duration::ZERO, now),
                    Epoch::new(13),
                    now
                )
                .await,
            Stood::Lost {
                granted: Epoch::ZERO
            },
            "a node just restarted counted a vote it had refused to cast"
        );
        drop(answering.join().expect("the door's thread"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_round_the_peers_carried_is_recorded_in_the_winners_own_memory() {
        // G053 SG2d, the kill test's lease lapse. This node granted a rival an
        // epoch moments ago, so its own voter refuses it the next one for a
        // whole TTL — and the two peers carry the round without it. A win its
        // own memory does not hold reads, at the standing gate, as a grant to
        // somebody else: the leader silenced itself for the rest of its lease
        // and never renewed. And past that TTL the same memory would grant the
        // NEXT epoch to a challenger while this node still writes under this one.
        let authority = Authority::new();
        let ids = [[73_u8; NODE_ID_LEN], [74_u8; NODE_ID_LEN]];
        let doors: Vec<_> = ids
            .iter()
            .map(|id| (*id, kept_voting(&authority, *id)))
            .collect();
        let peers: Vec<_> = doors
            .iter()
            .map(|(id, (address, _, _))| (*id, *address))
            .collect();

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let standing = standing(&mine, &der, &said, &peers);
        let now = Instant::now();
        let ours = mine_voting(now);
        let rival = [75_u8; NODE_ID_LEN];
        let rivals_grant = now
            .checked_sub(LEASE_TTL / 2)
            .expect("this machine has been up for a second");
        let earlier = ours.asked(
            &Ballot {
                epoch: Epoch::new(6),
                candidate: rival,
                range: tessari_types::Reach::Store,
            },
            rivals_grant,
            LEVEL,
            LEVEL,
        );
        assert_eq!(
            earlier,
            Vote::Granted,
            "the rival's grant this test starts from"
        );

        let won = standing
            .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(7), now)
            .await
            .won()
            .expect("both peers granted, which is two of three without this node");
        assert_eq!(won.epoch, Epoch::new(7));
        assert_eq!(
            ours.granted_elsewhere_at(THERE),
            None,
            "a leader reads its own win as a live grant to somebody else"
        );
        assert_eq!(ours.decided(), Some(Epoch::new(7)));

        let challenger = [76_u8; NODE_ID_LEN];
        // The rival's hold is over and the win's is not: only the win can
        // refuse this ballot now.
        let past_the_rivals_hold = rivals_grant.checked_add(LEASE_TTL).expect("representable");
        let vote = ours.asked(
            &Ballot {
                epoch: Epoch::new(8),
                candidate: challenger,
                range: tessari_types::Reach::Store,
            },
            past_the_rivals_hold,
            LEVEL,
            LEVEL,
        );
        assert!(
            matches!(vote, Vote::Refused(Refused::EarlierGrantStillAlive { .. })),
            "a sitting leader's own voter granted a challenger the next epoch: {vote:?}"
        );
        for (_, (_, _, answering)) in doors {
            answering.join().expect("the door's thread");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_round_carried_at_the_epoch_this_node_granted_a_rival_is_recorded() {
        // The second shape of the kill test's lapse. Three candidates stand at
        // one epoch; this node grants a rival that epoch at its door, then both
        // peers carry the same epoch for THIS node. Each voter grants an epoch
        // once, so the rival's bid at it lost — and a memory that kept naming
        // the rival silenced the winner exactly as a lower epoch did.
        let authority = Authority::new();
        let ids = [[77_u8; NODE_ID_LEN], [78_u8; NODE_ID_LEN]];
        let doors: Vec<_> = ids
            .iter()
            .map(|id| (*id, kept_voting(&authority, *id)))
            .collect();
        let peers: Vec<_> = doors
            .iter()
            .map(|(id, (address, _, _))| (*id, *address))
            .collect();

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let standing = standing(&mine, &der, &said, &peers);
        let now = Instant::now();
        let ours = mine_voting(now);
        let rival = [79_u8; NODE_ID_LEN];
        let earlier = ours.asked(
            &Ballot {
                epoch: Epoch::new(6),
                candidate: rival,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            LEVEL,
        );
        assert_eq!(
            earlier,
            Vote::Granted,
            "the rival's grant this test starts from"
        );

        let won = standing
            .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(6), now)
            .await
            .won()
            .expect("both peers granted, which is two of three without this node");
        assert_eq!(won.epoch, Epoch::new(6));
        assert_eq!(
            ours.granted_elsewhere_at(THERE),
            None,
            "a leader reads the rival's lost bid at its own epoch as a live grant"
        );
        for (_, (_, _, answering)) in doors {
            answering.join().expect("the door's thread");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_round_carried_below_an_epoch_this_node_already_granted_is_not_taken() {
        // The third shape, and the one that is not only liveness. While this
        // node canvassed for epoch 7, its door granted a rival epoch 8 — and
        // with it this node's log position as it stood. Leading at 7 after that
        // would append records that rival's line was promised it did not need,
        // acknowledged and then lost if the rival wins. Raft's rule: a node that
        // voted in a higher term is a follower in it, and an election for the
        // lower term is over whatever its replies say.
        let authority = Authority::new();
        let ids = [[80_u8; NODE_ID_LEN], [81_u8; NODE_ID_LEN]];
        let doors: Vec<_> = ids
            .iter()
            .map(|id| (*id, kept_voting(&authority, *id)))
            .collect();
        let peers: Vec<_> = doors
            .iter()
            .map(|(id, (address, _, _))| (*id, *address))
            .collect();

        let mine = authority.issue(THERE, Purpose::Peer);
        let der = authority.der();
        let said = hello(THERE);
        let standing = standing(&mine, &der, &said, &peers);
        let now = Instant::now();
        let ours = mine_voting(now);
        let rival = [82_u8; NODE_ID_LEN];
        let promised = ours.asked(
            &Ballot {
                epoch: Epoch::new(8),
                candidate: rival,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            LEVEL,
        );
        assert_eq!(
            promised,
            Vote::Granted,
            "the rival's grant this test starts from"
        );

        assert_eq!(
            standing
                .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(7), now)
                .await,
            Stood::Lost {
                granted: Epoch::new(8)
            },
            "a node took leadership of an epoch below one it had already granted a rival"
        );
        assert_eq!(
            ours.decided(),
            Some(Epoch::new(8)),
            "the grant to the rival was overwritten by a round it outranks"
        );
        for (_, (_, _, answering)) in doors {
            answering.join().expect("the door's thread");
        }
    }
}
