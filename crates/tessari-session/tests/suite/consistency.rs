//! The read's consistency level — one word naming the contract the read asks
//! for: `LEADER`, `SNAPSHOT`, `STALE WITHIN <span>`.
//!
//! # What a level is, and what it is not
//!
//! A level is the two read axes — authority and freshness — named together as
//! one word, with the anomaly contract that word carries stated in the
//! documentation. It is not a third axis: `LEADER` and `STALE WITHIN <span>`
//! desugar onto `ANSWERED BY LEADER` and `STALENESS <span>` and are enforced by
//! exactly the machinery those clauses have always had, which is why most of
//! the cases below are about the *refusals* — the level earns its place by
//! refusing where its contract cannot be met, not by answering anywhere new.
//! `SNAPSHOT` is the one level with no clause of its own: it names the promise
//! every statement already makes on a node holding everything it reads, and it
//! is refused wherever that promise cannot be kept — where the read would have
//! to gather a part from another node's present.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use core::time::Duration;

use tessari_constants::STALENESS_FLOOR_SECONDS;
use tessari_encoding::NODE_ID_LEN;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Elsewhere, Error, Peer, Session};
use tessari_storage::Store;
use tessari_types::Epoch;

use super::gathered_reads::{answer, pair, refused as refused_on_the_pair};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// A tenant with one record to read.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders SCHEMALESS;\n\
             CREATE orders:1 = { total: 10 };",
        )
        .unwrap();
    session
}

fn allowed() -> String {
    format!("{}s", STALENESS_FLOOR_SECONDS.saturating_mul(3))
}

const THERE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];
const THEIR_EPOCH: Epoch = Epoch::new(7);

/// A cluster of one peer, which may or may not claim it writes, whose copy is
/// `age` old. The same hand-written stub the staleness and answered-by suites
/// keep, for the same reason: the session's half is what these tests own.
#[derive(Debug)]
struct OnePeer {
    endpoint: String,
    leads: bool,
    age: Duration,
}

impl Elsewhere for OnePeer {
    fn within(&self, bound: Duration) -> Option<Peer> {
        (self.age <= bound).then(|| Peer {
            endpoint: self.endpoint.clone(),
            node: THERE,
            epoch: THEIR_EPOCH,
        })
    }

    fn serving(&self, _endpoint: &str, _node: &[u8; NODE_ID_LEN]) -> Option<Peer> {
        None
    }

    fn writable(&self) -> Option<Peer> {
        self.leads.then(|| Peer {
            endpoint: self.endpoint.clone(),
            node: THERE,
            epoch: THEIR_EPOCH,
        })
    }

    fn build_at(&self, _endpoint: &str) -> Option<tessari_encoding::NodeVersion> {
        None
    }

    fn leading(&self, _range: tessari_types::Reach) -> Option<Peer> {
        None
    }
}

fn peer(leads: bool, age: Duration) -> Arc<dyn Elsewhere> {
    Arc::new(OnePeer {
        endpoint: "two.example:9080".to_owned(),
        leads,
        age,
    })
}

/// A session on a node that is already `serving` only, the tenant in place.
fn reader(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// Level with the peer it collects from, as of now — through `Store::collected`,
/// the one way this node's currency is set anywhere.
fn level(store: &Store, reached: u64) {
    store.collected(
        tessari_types::Sequence::new(reached),
        tessari_storage::Currency::Level,
    );
}

#[test]
fn the_leader_level_is_the_authority_axis_named() {
    // Desugars to `ANSWERED BY LEADER`: a node that may write answers here; a
    // node that may not is sent to the one that claims it.
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("SELECT * FROM orders LEADER;")
        .expect("a node that may write is the leader of its own records");

    // Drained of the writable role, the same node redirects the level to the
    // peer that claims it.
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    let mut follower = reader(&store).among(peer(true, Duration::ZERO));
    let sent = follower
        .run_now("SELECT * FROM orders LEADER;")
        .expect_err("a follower is redirected, not answered");
    let Error::ReadIsElsewhere { because, .. } = &sent else {
        panic!("the level desugared into nothing: {sent}");
    };
    assert!(
        because.contains("LEADER"),
        "the refusal names the level that sent the client away: {because}"
    );
}

#[test]
fn the_stale_level_is_the_freshness_axis_named() {
    // Desugars to `STALENESS <span>`: the floor, the copy age, and the refusal
    // discipline are the clause's own, unchanged.
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(&format!("SELECT * FROM orders STALE WITHIN {};", allowed()))
        .expect("a leader is level with itself inside any bound it accepts");

    let refusal = session
        .run_now("SELECT * FROM orders STALE WITHIN 1s;")
        .unwrap_err();
    assert!(
        matches!(refusal, Error::StalenessBelowFloor { .. }),
        "the level is held to the same floor as the clause: {refusal}"
    );

    // A follower whose copy levelled a moment ago answers here, inside a bound
    // three floors wide — the same comparison the staleness suite pins.
    session.run_now("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);
    let mut reading = reader(&store).among(peer(false, Duration::ZERO));
    let outcomes = reading
        .run_now(&format!("SELECT * FROM orders STALE WITHIN {};", allowed()))
        .expect("a copy level a moment ago is inside a bound three floors wide");
    assert_eq!(outcomes.len(), 1, "and it is answered here, not redirected");
}

#[test]
fn the_snapshot_level_answers_on_a_node_holding_everything() {
    // On a full holder the statement's own committed point already satisfies
    // the level: the marker changes nothing, which is the point.
    let store = store();
    let mut session = ready(&store);

    let outcomes = session
        .run_now("SELECT * FROM orders SNAPSHOT;")
        .expect("one node holding the whole table is one point");

    assert_eq!(outcomes.len(), 1);
}

#[test]
fn the_leader_level_beside_answered_by_any_is_refused() {
    // The level demands the one node that decides writes; the clause admits
    // any copy. There is no reading of the pair that is not a guess, and the
    // guess would fail in the unsafe direction. The level sits at the
    // statement's tail, after the clauses it can disagree with.
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run_now("SELECT * FROM orders ANSWERED BY ANY LEADER;")
        .unwrap_err();

    let message = refusal.to_string();
    assert!(
        message.contains("ANY"),
        "the refusal echoes the clause that contradicts the level: {message}"
    );
    assert!(
        message.contains("LEADER"),
        "and it names the level the clause contradicts: {message}"
    );
}

#[test]
fn the_leader_level_beside_answered_by_leader_agrees() {
    // Redundant but agreeing: both name the authority axis, and agreement is
    // not a contradiction. The statement says the same thing twice.
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("SELECT * FROM orders ANSWERED BY LEADER LEADER;")
        .expect("two names for one axis, said in agreement");
}

#[test]
fn the_stale_level_beside_answered_by_leader_is_legal() {
    // The two axes stay independent under a level: authority and freshness
    // compose exactly as the two clauses always have.
    let store = store();
    let mut session = ready(&store);

    session
        .run_now(&format!(
            "SELECT * FROM orders ANSWERED BY LEADER STALE WITHIN {};",
            allowed()
        ))
        .expect("the level that names freshness composes with authority");
}

#[test]
fn a_level_beside_a_version_is_refused() {
    // `VERSION` names one exact point in history. A level beside it is a
    // second answer to a question already answered — for `SNAPSHOT` literally,
    // and for the others because the level's contract has no room left.
    let store = store();
    let mut session = ready(&store);

    for read in [
        "SELECT * FROM orders SNAPSHOT VERSION 1;",
        "SELECT * FROM orders LEADER VERSION 1;",
        "SELECT * FROM orders STALE WITHIN 30s VERSION 1;",
    ] {
        let refusal = session.run_now(read).unwrap_err();
        assert!(
            refusal.to_string().contains("VERSION"),
            "{read}: the refusal says which two ideas disagree: {refusal}"
        );
    }
}

#[test]
fn the_stale_level_needs_its_span() {
    // No stored default exists: a level without its bound would mean a
    // tolerance the statement never wrote down.
    let store = store();
    let mut session = ready(&store);

    for read in [
        "SELECT * FROM orders STALE;",
        "SELECT * FROM orders STALE 30s;",
    ] {
        let refusal = session.run_now(read).unwrap_err();
        assert!(
            refusal.to_string().contains("WITHIN"),
            "{read}: the refusal names what would make it acceptable: {refusal}"
        );
    }
}

#[test]
fn the_stale_level_beside_staleness_is_refused() {
    // The level carries its own bound; a second bound beside it is two
    // answers to one question. (`STALENESS` may still refine the levels that
    // carry no bound of their own — `LEADER`, `SNAPSHOT` — exactly as it
    // always has; the level comes after it at the statement's tail.)
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run_now(&format!(
            "SELECT * FROM orders STALENESS 1m STALE WITHIN {};",
            allowed()
        ))
        .unwrap_err();
    assert!(
        refusal.to_string().contains("STALENESS"),
        "the refusal names the clause that repeats the bound: {refusal}"
    );

    session
        .run_now(&format!(
            "SELECT * FROM orders STALENESS {} LEADER;",
            allowed()
        ))
        .expect("a bound refines a level that carries none, as it always has");
    session
        .run_now(&format!(
            "SELECT * FROM orders STALENESS {} SNAPSHOT;",
            allowed()
        ))
        .expect("and the same beside the snapshot level");
}

#[test]
fn an_unknown_level_word_is_refused_where_the_statement_is_read() {
    // A word at the level's place that names no level refuses rather than
    // defaulting — the direction a guess would fail in is the unsafe one.
    let store = store();
    let mut session = ready(&store);

    let refusal = session.run_now("SELECT * FROM orders FROZEN;").unwrap_err();
    assert!(
        refusal.to_string().contains("FROZEN"),
        "the refusal echoes what was written: {refusal}"
    );
}

#[test]
fn the_snapshot_level_on_a_partial_holder_is_refused_not_gathered() {
    // The one place the snapshot promise cannot be kept: a gathered part would
    // be another node's present, so the level refuses exactly where a
    // transaction or `VERSION` read refuses — never answered as a mix of two
    // points.
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");

    let error = refused_on_the_pair(&mut follower, "SELECT * FROM ledger SNAPSHOT;");
    let Error::NotHeldHere { table, shards, .. } = &error else {
        panic!("the level was answered weaker than it was named: {error}");
    };
    assert_eq!(table, "ledger");
    assert_eq!(
        shards,
        &[1, 3],
        "the shards this node does not hold, by name"
    );

    // The control: the same statement without the level gathers, as it always
    // has — the level, not the table, is what refused.
    let mut again = pair.on_the_follower("reader");
    let (records, _) = answer(&mut again, "SELECT * FROM ledger;");
    assert!(!records.is_empty(), "gathering itself is untouched");
}

#[test]
fn the_snapshot_level_inside_a_transaction_is_legal() {
    // A transaction is one snapshot already; the level restates the promise
    // rather than naming a second point (which is what `VERSION` inside one
    // refuses for).
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("BEGIN; SELECT * FROM orders SNAPSHOT; COMMIT;")
        .expect("a promise restated is not a contradiction");
}

#[test]
fn a_field_called_leader_is_still_a_field() {
    // The level words are contextual, the way `staleness` is: they open a
    // level only at the statement's tail, and read as names everywhere a name
    // can sit.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE tickets SCHEMALESS;\n\
             CREATE tickets:1 = { leader: true, snapshot: 'yes', stale: 1 };",
        )
        .unwrap();

    session
        .run_now("SELECT leader, snapshot, stale FROM tickets;")
        .expect("a word that opens a level only in a level's place");
    session
        .run_now("SELECT * FROM tickets WHERE leader = true AND stale > 0;")
        .expect("and in a condition the words are the fields they always were");
}
