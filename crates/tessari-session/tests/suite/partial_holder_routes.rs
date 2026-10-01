//! G051 C4 — a node holding part of a split table, refused `NotHeldHere`,
//! names a node holding the whole of it when it knows one (ADR-0101).
//!
//! # What "holds the whole table" means here
//!
//! A member row whose subscription covers the table's database: the same
//! question `Session::missing` asks of this node's own served reach, asked of
//! the peer's declared one. The directory then says whether that node was heard
//! serving, and what epoch it claimed — a row nobody has heard from is a node
//! of no known state, and naming it would send a client somewhere that may not
//! answer.
//!
//! The directory is hand-written for the reason `staleness.rs` gives: the real
//! one lives in `tessari-wire`, which depends on this crate. That it is asked,
//! and that its answer is used, is the session's half and is what these assert.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_session::{Elsewhere, Peer};
use tessari_storage::{Catalog, Reach, ReplicaDefinition, Store};
use tessari_types::{DatabaseId, Epoch, NamespaceId, ShardId};

use crate::gathered_reads::{follower_of_the_middle, leader, signed_in};

const WHOLE: [u8; NODE_ID_LEN] = [5; NODE_ID_LEN];
const WHOLE_AT: &str = "whole.example:9180";
/// What that node claimed for itself — a value the partial holder could not
/// have produced, so a redirect carrying it carries what the directory heard.
const ITS_EPOCH: Epoch = Epoch::new(11);

/// A directory that has heard exactly one node, `node`, serving at `WHOLE_AT`.
#[derive(Debug)]
struct Heard {
    node: [u8; NODE_ID_LEN],
}

impl Elsewhere for Heard {
    fn writable(&self) -> Option<Peer> {
        None
    }

    fn within(&self, _bound: Duration) -> Option<Peer> {
        None
    }

    fn serving(&self, endpoint: &str, node: &[u8; NODE_ID_LEN]) -> Option<Peer> {
        (endpoint == WHOLE_AT && *node == self.node).then(|| Peer {
            endpoint: endpoint.to_owned(),
            node: *node,
            epoch: ITS_EPOCH,
        })
    }
}

/// Declare a peer at `WHOLE_AT` that is `node` and collects `reach`.
fn declare(store: &Store, node: [u8; NODE_ID_LEN], reach: Reach) {
    let mut transaction = store.begin().unwrap();
    Catalog::new(&mut transaction)
        .create_replica_with(ReplicaDefinition {
            name: "whole".to_owned(),
            endpoint: WHOLE_AT.to_owned(),
            roles: Roles::SERVING,
            node: Some(node),
            replicates: Some(reach),
            leads: None,
            clients: None,
            http: None,
        })
        .unwrap();
    transaction.commit().unwrap();
}

const fn the_database() -> Reach {
    Reach::Database(NamespaceId::new(1), DatabaseId::new(1))
}

/// The holder a `NotHeldHere` refusal of `read` named, with `heard` known.
fn named(follower: &Store, heard: [u8; NODE_ID_LEN], read: &str) -> Option<Peer> {
    let mut session = signed_in(follower, "root").among(Arc::new(Heard { node: heard }));
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    match session.run(read) {
        Err(tessari_session::Error::NotHeldHere { table, holder, .. }) => {
            assert_eq!(table, "ledger", "{read}");
            holder
        }
        other => panic!("{read}: expected NotHeldHere, got {other:?}"),
    }
}

/// Every source that refuses rather than gathers: no gatherer at all, inside a
/// transaction, under `VERSION`, a `FETCH` into a missing shard, a join side,
/// and the read every `DELETE` and `UPDATE` makes before it writes — which,
/// unrefused, removed only the records this node holds and reported success.
const REFUSED: [&str; 9] = [
    "SELECT * FROM ledger;",
    "BEGIN; SELECT * FROM ledger; COMMIT;",
    "SELECT * FROM ledger VERSION 5;",
    "SELECT * FROM ledger:'h' FETCH peer;",
    "SELECT * FROM other JOIN ledger ON other.total = ledger.total;",
    "DELETE FROM ledger WHERE total > 0 LIMIT ALL;",
    "DELETE FROM ledger:'a'..'z' LIMIT ALL;",
    "DELETE ledger:'a';",
    "UPDATE ledger:'a' SET total = 1;",
];

#[test]
fn a_partial_holder_names_a_node_holding_the_whole_database() {
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    declare(&follower, WHOLE, the_database());
    for read in REFUSED {
        assert_eq!(
            named(&follower, WHOLE, read),
            Some(Peer {
                endpoint: WHOLE_AT.to_owned(),
                node: WHOLE,
                epoch: ITS_EPOCH,
            }),
            "{read}"
        );
    }
}

#[test]
fn a_node_holding_one_shard_is_not_named() {
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    let mut transaction = leader.begin().unwrap();
    let ledger = Catalog::new(&mut transaction)
        .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
        .unwrap()
        .unwrap();
    drop(transaction);
    declare(
        &follower,
        WHOLE,
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            ledger,
            ShardId::new(1),
        ),
    );
    for read in REFUSED {
        assert_eq!(named(&follower, WHOLE, read), None, "{read}");
    }
}

#[test]
fn a_whole_holder_nobody_has_heard_from_is_not_named() {
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    declare(&follower, WHOLE, the_database());
    // The directory heard a different node at that address.
    for read in REFUSED {
        assert_eq!(named(&follower, [6; NODE_ID_LEN], read), None, "{read}");
    }
}

#[test]
fn a_node_is_never_named_as_the_holder_it_is_not() {
    // A row about this node itself, claiming the whole database, while its
    // served reach is one shard: naming it would send the client back here.
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    let me = follower.node_identity().unwrap().id;
    declare(&follower, me, the_database());
    for read in REFUSED {
        assert_eq!(named(&follower, me, read), None, "{read}");
    }
}
