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

mod epochs;
mod lines;
mod rounds;
mod voters;

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

fn shard(n: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        ShardId::new(n),
    )
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

/// A ballot for `candidate` at `epoch` on the store's line.
fn store_ballot(epoch: u64, candidate: [u8; NODE_ID_LEN]) -> Ballot {
    Ballot {
        epoch: Epoch::new(epoch),
        candidate,
        range: Reach::Store,
    }
}
