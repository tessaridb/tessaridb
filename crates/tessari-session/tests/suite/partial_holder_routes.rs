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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_session::{Elsewhere, Outcome, Peer};
use tessari_storage::{Catalog, Reach, ReplicaDefinition, Store};
use tessari_types::{DatabaseId, Epoch, NamespaceId, ShardId, Value};

use crate::gathered_reads::{Moved, follower_of_the_middle, leader, signed_in};

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

    fn build_at(&self, _endpoint: &str) -> Option<tessari_encoding::NodeVersion> {
        None
    }

    fn leading(&self, _range: tessari_types::Reach) -> Option<Peer> {
        None
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
            fingerprint: None,
            join: None,
            releasing: false,
            preferred: false,
            region: None,
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
    let before = follower.health().unwrap().not_held_here;
    match session.run(read) {
        Err(tessari_session::Error::NotHeldHere { table, holder, .. }) => {
            assert_eq!(table, "ledger", "{read}");
            // G053 C6: each refusal is counted once, where it is made.
            assert_eq!(
                Some(follower.health().unwrap().not_held_here),
                before.checked_add(1),
                "{read}"
            );
            holder
        }
        other => panic!("{read}: expected NotHeldHere, got {other:?}"),
    }
}

/// Every source that refuses on a node told of no gatherer — the plain read, a
/// `FETCH` into a missing shard and a join side (each gathered where a gatherer
/// is, G057 C2) — or where gathering is withheld: inside a transaction, under
/// `VERSION`; and the read every `DELETE` and `UPDATE` makes before it writes — which,
/// unrefused, removed only the records this node holds and reported success.
const REFUSED: [&str; 9] = [
    "SELECT * FROM ledger;",
    "BEGIN; SELECT * FROM ledger; COMMIT;",
    "SELECT * FROM ledger VERSION 5;",
    "SELECT * FROM ledger:'h' FETCH peer;",
    "SELECT * FROM ledger JOIN ledger AS twin ON ledger.note = twin.note;",
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

/// SG3 — a gathered read whose leader holds a different map of the table
/// names the whole holder too: the read this node cannot answer now is one that
/// node answers, so a surface can send the client there (a transient redirect)
/// instead of making it parse a refusal and retry by hand.
#[test]
fn a_moved_map_names_a_node_holding_the_whole_database() {
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    let holder_of = |heard: [u8; NODE_ID_LEN]| {
        let mut session = signed_in(&follower, "root")
            .among(Arc::new(Heard { node: heard }))
            .gathering(Arc::new(Moved));
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        match session.run("SELECT * FROM ledger;") {
            Err(tessari_session::Error::ShardMapMoved { table, holder, .. }) => {
                assert_eq!(table, "ledger");
                holder
            }
            other => panic!("expected ShardMapMoved, got {other:?}"),
        }
    };
    assert_eq!(holder_of(WHOLE), None, "no peer declared yet");
    declare(&follower, WHOLE, the_database());
    assert_eq!(
        holder_of(WHOLE),
        Some(Peer {
            endpoint: WHOLE_AT.to_owned(),
            node: WHOLE,
            epoch: ITS_EPOCH,
        })
    );
    assert_eq!(holder_of([6; NODE_ID_LEN]), None, "a node nobody heard");
}

/// SG3 — `session::context()` is what a client following a redirect asks on
/// both ends: which node it is talking to, and which tenancy its session had
/// selected there. Any session may ask, a reader included: the node id is what
/// every redirect frame already discloses and the tenancy is the caller's own.
#[test]
fn a_session_reads_its_node_and_its_own_tenancy() {
    let leader = leader();
    let me = leader.node_identity().unwrap().id;
    let mut session = signed_in(&leader, "reader");
    let context = |session: &mut tessari_session::Session<'_>| match session
        .run("RETURN session::context();")
        .unwrap()
        .pop()
    {
        Some(Outcome::Value(value)) => value,
        other => panic!("expected a value, got {other:?}"),
    };
    let expected = |namespace: Value, database: Value| {
        Value::Object(BTreeMap::from([
            ("node".to_owned(), Value::Uuid(me)),
            ("namespace".to_owned(), namespace),
            ("database".to_owned(), database),
        ]))
    };
    assert_eq!(context(&mut session), expected(Value::Null, Value::Null));
    session.run("USE NAMESPACE prod;").unwrap();
    assert_eq!(
        context(&mut session),
        expected(Value::from("prod"), Value::Null)
    );
    session.run("USE DATABASE shop;").unwrap();
    assert_eq!(
        context(&mut session),
        expected(Value::from("prod"), Value::from("shop"))
    );
}
