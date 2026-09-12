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
use tessari_types::Epoch;

use crate::grant::{Leadership, Round};
use crate::link::{Answered, Ask, Credential, call};
use crate::peer::Hello;

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
    #[must_use]
    pub fn renew(&self, held: Lease, next: Epoch, now: Instant) -> Option<Leadership> {
        if renew_in(held, self.round, now) > Duration::ZERO {
            return None;
        }
        let mut round = Round::opened_at(next, self.candidate, self.peers.len(), now);
        for (peer, address) in self.peers {
            let Ok((_, answered)) = call(
                *address,
                duplicate(self.mine),
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
            if let Some(won) = round.counts(*peer, vote) {
                return Some(won);
            }
        }
        None
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

/// A second copy of this node's credential, for the next door.
///
/// The call takes ownership because a TLS client configuration does, and a
/// canvass speaks to every member. Private on purpose: duplicating key material
/// is a detail of speaking to N peers rather than a capability worth publishing.
fn duplicate(mine: &Credential) -> Credential {
    Credential {
        chain: mine.chain.clone(),
        key: mine.key.clone_key(),
    }
}

#[cfg(test)]
mod tests {
    use super::Standing;
    use crate::grant::{Ballot, Vote};
    use crate::link::tests::{Authority, THERE, hello, settled, voting};
    use crate::link::{Answered, Ask, Credential, Peers, call};
    use crate::peer::{Hello, Purpose};
    use rustls::pki_types::CertificateDer;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};
    use tessari_encoding::NODE_ID_LEN;
    use tessari_storage::{LEASE_GUARD, Lease};
    use tessari_types::Epoch;

    /// A round that takes a tenth of a second — long enough that two of them is
    /// a margin a test can state, short enough that every case here runs at
    /// once.
    const ROUND: Duration = Duration::from_millis(100);

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
        }
    }

    /// A lease with exactly `margin` of writable time left as of `now`.
    fn leaving(margin: Duration, now: Instant) -> Lease {
        Lease::taken_at(now, LEASE_GUARD.checked_add(margin).expect("representable"))
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
            standing.renew(held, Epoch::new(2), now),
            None,
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
            .renew(held, Epoch::new(2), now)
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
            .renew(leaving(Duration::ZERO, now), Epoch::new(7), now)
            .expect("a majority of three granted it");
        let answered = Instant::now();

        assert_eq!(won.epoch, Epoch::new(7));
        // Dated from when the round opened, not from when the majority came
        // back. The gap between the two is real — three TLS handshakes happened
        // in it — and it is charged to this node's own window rather than to the
        // voters', which is the rule the seam exists to carry.
        assert_eq!(won.from, now, "dated from the instant the round opened");
        assert!(answered > now, "and the collection delay was not nothing");

        // A majority of three is two, so the round ended at the second door and
        // the third was never asked. That is not an accident of iteration order
        // — it is the round concluding the moment it is carried — and it shows
        // in the third voter's untouched memory: the epoch is still grantable,
        // which it would not be had the ballot reached it.
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
            .renew(leaving(Duration::ZERO, now), Epoch::new(3), now)
            .expect("two of three is a majority, and the third was never needed");
        assert_eq!(won.epoch, Epoch::new(3));

        for (_, (_, door)) in doors {
            drop(door.join().expect("the door's thread"));
        }
    }
}
