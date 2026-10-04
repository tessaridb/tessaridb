use super::{Standing, Stood};
use crate::grant::{Ballot, Deciding, Refused, Vote, Voter};
use crate::keys::PeerKeys;
use crate::link::tests::{Authority, THERE, hello, settled, voting};
use crate::link::{Answered, Ask};
use crate::peer::{Hello, Purpose};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{LEASE_GUARD, LEASE_TTL, Lease};
use tessari_types::Epoch;

mod recorded;
mod renewal;
mod rounds;

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
    keys: &'a PeerKeys,
    said: &'a Hello,
    peers: &'a [([u8; NODE_ID_LEN], SocketAddr)],
) -> Standing<'a> {
    Standing {
        candidate: THERE,
        keys,
        said,
        peers,
        round: ROUND,
        range: tessari_types::Reach::Store,
        lease: LEASE_TTL,
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
    let door = crate::link::tests::bind_with(
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
