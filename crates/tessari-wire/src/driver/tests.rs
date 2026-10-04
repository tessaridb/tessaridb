use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::Lease;
use tessari_types::{Epoch, NamespaceId, Reach, Sequence};
use tokio_util::sync::CancellationToken;

use tessari_storage::{FailoverStamp, ReplicaDefinition};

use super::{
    Collecting, Published, Renewing, Seed, bootstrap_from, campaign_line, campaigns_for, due_in,
    election_timeout, every, heard_a_leader, heard_a_leader_on, heard_a_newer_policy,
    leader_of_range, names_a_peer, preferred_to_yield_to, released, stands, stands_for,
    stands_for_the_store, upstream, voters,
};
use crate::campaign::Stood;
use crate::directory::Directory;
use crate::grant::Leadership;
use crate::peer::Hello;

mod cadence;
mod collecting;
mod lease;
mod ranges;
mod routing;
mod standing;

const NODE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];
/// The log every single-log fixture here counts in.
const STORE: Reach = Reach::Store;
const ANOTHER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];

/// A serving peer one second behind.
fn said() -> Hello {
    Hello {
        node: NODE,
        build: NodeVersion {
            major: 0,
            minor: 1,
            patch: 1,
        },
        epoch: Epoch::new(7),
        roles: Roles::SERVING,
        tail: Sequence::new(4096),
        tail_leadership: Epoch::new(7),
        current_as_of: Some(Duration::from_secs(1)),
        policy: None,
        line: None,
    }
}

/// A greeting from a node that may write right now.
///
/// `current_as_of` answering `Some(0)` is exactly what a node says when its
/// EFFECTIVE roles carry `writable`: it is the origin of what it holds, so
/// there is nothing for it to be stale relative to.
fn writing(epoch: Epoch) -> Hello {
    Hello {
        epoch,
        current_as_of: Some(Duration::ZERO),
        ..said()
    }
}

/// A greeting from a node that holds somebody else's writes.
fn following() -> Hello {
    said()
}

/// A directory holding one greeting per endpoint, all heard just now.
fn greeted(rows: &[(&str, Hello)]) -> Directory {
    let mut directory = Directory::new();
    let now = Instant::now();
    for (endpoint, said) in rows {
        directory.heard(endpoint, *said, now);
    }
    directory
}

/// A declared peer row naming a node at an address, writable and
/// coordinating — the shape every member of a cluster that can fail over
/// carries for every other member.
fn named(name: &str, endpoint: &str, node: [u8; NODE_ID_LEN]) -> ReplicaDefinition {
    ReplicaDefinition {
        name: name.to_owned(),
        endpoint: endpoint.to_owned(),
        roles: Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
        node: Some(node),
        ..peer(Roles::WRITABLE, Some(node))
    }
}

/// A declared peer row, as an operator would have written it.
fn peer(roles: Roles, node: Option<[u8; NODE_ID_LEN]>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: "leader".to_owned(),
        endpoint: "10.0.0.2:9000".to_owned(),
        roles,
        node,
        // Not read by `upstream` and set anyway: what the peer grants *this*
        // node lives on that peer's own catalog, not on this node's copy of
        // the row, and a value here that mattered would mean the follower
        // was deciding its own subscription.
        replicates: None,
        leads: None,
        clients: None,
        http: None,
        fingerprint: None,
        join: None,
        releasing: false,
        preferred: false,
        region: None,
    }
}

/// A seed, as an operator would have written it on the command line.
fn seed(node: [u8; NODE_ID_LEN], endpoint: &str) -> Seed {
    Seed {
        node,
        endpoint: endpoint.to_owned(),
    }
}

/// A greeting from a node running the policy set at `(epoch, version)`.
fn running(epoch: u64, version: u64) -> Hello {
    Hello {
        policy: Some(FailoverStamp {
            epoch: Epoch::new(epoch),
            version,
        }),
        ..said()
    }
}

fn stamp(epoch: u64, version: u64) -> FailoverStamp {
    FailoverStamp {
        epoch: Epoch::new(epoch),
        version,
    }
}

fn shard(n: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        tessari_types::DatabaseId::new(1),
        tessari_types::TableId::new(1),
        tessari_types::ShardId::new(n),
    )
}

/// A greeting from `node` standing for `range`, leading it at `leading`.
fn on_a_line(node: [u8; NODE_ID_LEN], range: Reach, leading: u64) -> Hello {
    Hello {
        node,
        line: Some(crate::peer::Line {
            range,
            leading: Epoch::new(leading),
            tail: Sequence::new(3),
            tail_leadership: Epoch::new(1),
        }),
        ..following()
    }
}
