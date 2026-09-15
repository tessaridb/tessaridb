//! `STALENESS` — a bound tighter than what this cluster can know is refused.
//!
//! G024 **S6.2**, the half that needs no peers. The criterion has two: a read
//! carrying a staleness bound is routed away from nodes beyond it, and a bound
//! tighter than the awareness interval is refused at the API with the floor
//! named. Routing needs somewhere to route; the refusal does not.
//!
//! # Why a floor exists
//!
//! A bound says how far behind an answering node may be. A bound tighter than
//! the interval at which this node learns anything about its peers would be
//! enforced against a picture whose own age exceeds the tolerance — a promise
//! nothing can check.
//!
//! The floor's *value* and the prior art behind it live with the constant that
//! owns them, `tessari_constants::STALENESS_FLOOR_SECONDS`, and are not restated
//! here. Every value in this file is derived from that constant rather than
//! written out, so the suite asserts the rule and never the number — which is
//! what a constant expected to change when the control round lands needs.
//!
//! # The refusal has to name the floor
//!
//! Asserted here rather than left to the message's wording. A caller told only
//! that their bound was too tight cannot write a statement that would be
//! accepted; a caller told the floor can. That is the difference between a
//! refusal and an obstruction, and it is C-05's own requirement.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use core::time::Duration;

use tessari_constants::STALENESS_FLOOR_SECONDS;
use tessari_encoding::NODE_ID_LEN;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Elsewhere, Error, Peer, Session};
use tessari_storage::Store;
use tessari_types::Epoch;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A tenant with one record to read.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders SCHEMALESS;\n\
             CREATE orders:1 = { total: 10 };",
        )
        .unwrap();
    session
}

/// A bound comfortably above the floor, written the way a statement would.
fn allowed() -> String {
    format!("{}s", STALENESS_FLOOR_SECONDS.saturating_mul(3))
}

/// The node the made-up peer is, so the redirect has something to be checked
/// against.
const THERE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

/// The leadership that peer last claimed for itself.
///
/// A value this node could not have produced on its own, which is the point:
/// the redirect has to carry what the NAMED node said, not what this one holds.
const THEIR_EPOCH: Epoch = Epoch::new(7);

/// A cluster of exactly one peer, whose copy is `age` old.
///
/// Hand-written rather than a real `Directory`, and not because a stub is
/// easier: `tessari-wire` depends on this crate and not the reverse, so a
/// directory is not nameable here at all. What these tests own is the
/// SESSION's half — that it asks, and that it uses the answer. That the
/// directory picks the right peer is asserted where the directory lives.
#[derive(Debug)]
struct OnePeer {
    endpoint: String,
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
}

/// One peer, `age` behind, at an address a redirect can name.
fn one_peer(age: Duration) -> std::sync::Arc<dyn Elsewhere> {
    std::sync::Arc::new(OnePeer {
        endpoint: "two.example:9080".to_owned(),
        age,
    })
}

#[test]
fn a_bound_above_the_floor_is_answered_here() {
    // A leader's own answer is never stale relative to itself, so the bound is
    // satisfied rather than ignored. The clause does something today; it is the
    // routing that has nowhere to go.
    let store = store();
    let mut session = ready(&store);

    let outcomes = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect("this node is within any bound it can accept");

    assert_eq!(outcomes.len(), 1);
}

#[test]
fn a_bound_below_the_floor_is_refused_and_the_floor_is_named() {
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run("SELECT * FROM orders STALENESS 1s;")
        .unwrap_err();

    let Error::StalenessBelowFloor { floor, written, .. } = &refusal else {
        panic!("refused for the wrong reason: {refusal}");
    };
    assert_eq!(*floor, STALENESS_FLOOR_SECONDS);
    assert_eq!(written, "1s");
    assert!(
        refusal
            .to_string()
            .contains(&format!("{STALENESS_FLOOR_SECONDS}s")),
        "a caller must be able to write an acceptable statement from the refusal: {refusal}"
    );
}

#[test]
fn the_floor_itself_is_accepted() {
    // The boundary, asserted in the direction that would otherwise drift: a
    // floor refusing the value it names is a floor nobody can satisfy.
    let store = store();
    let mut session = ready(&store);

    session
        .run(&format!(
            "SELECT * FROM orders STALENESS {STALENESS_FLOOR_SECONDS}s;"
        ))
        .expect("the floor is the tightest bound that is accepted, not the tightest refused");
}

#[test]
fn a_bound_of_nothing_is_refused_where_the_statement_is_read() {
    // Not at the floor and not at the read: a tolerance of zero admits no node
    // at all, including this one, so the clause could only ever refuse — which
    // is a mistake in the statement.
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run("SELECT * FROM orders STALENESS 0s;")
        .unwrap_err();
    assert!(
        refusal.to_string().contains("staleness"),
        "refused by name, as it is read: {refusal}"
    );
    assert!(
        !matches!(refusal, Error::StalenessBelowFloor { .. }),
        "and refused by the parser rather than by the cluster's floor"
    );
}

#[test]
fn a_staleness_bound_cannot_qualify_a_read_that_names_a_version() {
    // `VERSION` names one exact point in this store's history. A tolerance for
    // how old that point may be is not a narrowing of it — it is a second answer
    // to a question already answered, and there is no reading of the pair that
    // is not a guess about which was meant.
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run(&format!(
            "SELECT * FROM orders VERSION 1 STALENESS {};",
            allowed()
        ))
        .unwrap_err();
    assert!(
        refusal.to_string().contains("VERSION"),
        "the refusal says which two clauses disagree: {refusal}"
    );
}

#[test]
fn a_field_called_staleness_is_still_a_field() {
    // The clause word is contextual, the way `TIMEOUT`'s is: it opens a clause
    // only when a duration follows it. Without this, adding a clause would
    // silently break every schema that had already used the word as a name.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE readings SCHEMALESS;\n\
             CREATE readings:1 = { staleness: 4 };",
        )
        .unwrap();

    session
        .run("SELECT staleness FROM readings;")
        .expect("`staleness` with no duration after it is an ordinary field name");
}

#[test]
fn a_node_whose_copy_has_no_known_age_refuses_a_bounded_read() {
    // §C-05's *exclude, never mark*, at the only candidate that exists. A node
    // that may not write holds a copy of somebody else's writes, and nothing in
    // this build can say how old that copy is — so it is outside every bound
    // rather than inside the ones it might happen to satisfy.
    //
    // The unbounded read beside it is what makes this about the BOUND. Without
    // it the test would pass just as well against a node that had stopped
    // answering reads altogether.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE NODE ROLES serving;").unwrap();

    session
        .run("SELECT * FROM orders;")
        .expect("a read that names no tolerance is unaffected");

    let refusal = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("a copy with no known age is beyond every bound");

    assert!(
        matches!(refusal, Error::NoCopyWithinStaleness { .. }),
        "expected the staleness refusal, got {refusal:?}"
    );
}

#[test]
fn a_leader_whose_lease_lapsed_is_no_longer_a_copy_within_any_bound() {
    // The one case that separates *the roles this node adopted* from *the roles
    // its lease leaves it*. A node configured `writable` whose fence has shut
    // still reads `writable` in the adopted set, and answering a bounded read
    // from it would serve data that somebody else may already have written past.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE NODE ROLES serving, writable;").unwrap();

    session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect("while the fence is open this node is its own origin");

    store.hold_lease(core::time::Duration::ZERO);

    let refusal = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("a lapsed leader is no longer the origin of what it holds");

    assert!(
        matches!(refusal, Error::NoCopyWithinStaleness { .. }),
        "expected the staleness refusal, got {refusal:?}"
    );
}

#[test]
fn the_bounded_refusal_blames_the_cluster_and_not_the_statement() {
    // The decision this refusal carries, asserted rather than left to wording
    // that a later edit could quietly invert. The bound cleared the floor, so
    // the statement was never wrong; what is missing is a copy young enough.
    //
    // And it says what it did NOT do — §C-05's other half. A caller who cannot
    // tell a refusal from a silent promotion has no way to know whether the
    // leader is now carrying their read.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let refusal = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("a copy with no known age is beyond every bound");
    let said = refusal.to_string();

    // `Duration::to_literal` normalises, so the refusal spells the tolerance
    // the engine's way rather than the statement's. Derived from the same
    // constant as `allowed()` so the suite still asserts no number of its own.
    let as_written = tessari_types::Duration::from_seconds(
        i64::try_from(STALENESS_FLOOR_SECONDS.saturating_mul(3)).unwrap(),
    )
    .to_literal();
    assert!(
        said.contains(&as_written),
        "the refusal names the bound that was asked for ({as_written}): {said}"
    );
    assert!(
        said.contains("rather than sent to the leader"),
        "the refusal says the read was not promoted: {said}"
    );
    assert!(
        !said.contains("floor"),
        "this bound cleared the floor, so the refusal must not read as that one: {said}"
    );
}

#[test]
fn a_bounded_read_this_node_cannot_answer_is_sent_to_a_peer_that_can() {
    // S6.2's routing half, and the whole of what this wave is for. The node
    // itself may not write, so its copy is of no known age and outside every
    // bound; a peer well inside the bound exists; C-07 says this node answers
    // by naming the one that should rather than fetching on the client's
    // behalf.
    let store = store();
    let mut session = ready(&store).among(one_peer(Duration::from_secs(1)));
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let sent = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("a redirect is an answer, and it arrives as one of these");

    let Error::ReadIsElsewhere {
        endpoint,
        node,
        epoch,
        ..
    } = &sent
    else {
        panic!("answered the wrong way: {sent}");
    };
    assert_eq!(endpoint, "two.example:9080");
    assert_eq!(
        *node, THERE,
        "a redirect naming only a place cannot be checked on arrival"
    );
    // Undated, a client that followed a redirect written under an old
    // leadership would arrive, be redirected again, and have no way to tell a
    // loop from progress.
    assert_eq!(
        *epoch, THEIR_EPOCH,
        "the redirect lost the leadership the named node claimed"
    );
}

#[test]
fn a_redirect_says_that_this_node_did_not_fetch_on_the_callers_behalf() {
    // C-07's *no node proxies*, asserted rather than left to wording a later
    // edit could quietly invert. A caller who cannot tell a redirect from a
    // silent proxy has no way to know whether this node is now holding their
    // read open against a peer.
    let store = store();
    let mut session = ready(&store).among(one_peer(Duration::from_secs(1)));
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let said = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("this node cannot answer it")
        .to_string();

    assert!(
        said.contains("two.example:9080"),
        "the redirect names where to go: {said}"
    );
    assert!(
        said.contains("redirects rather than fetching on your behalf"),
        "the redirect says what it did not do: {said}"
    );
    assert!(
        !said.contains("rather than sent to the leader"),
        "this is a redirect, so it must not read as the refusal: {said}"
    );
}

#[test]
fn a_peer_beyond_the_bound_leaves_the_read_refused_and_not_redirected() {
    // The other half of *exclude, never mark*. A cluster that knows of a peer
    // and knows it is too far behind must refuse exactly as a cluster that
    // knows of nobody — otherwise the bound is advisory.
    let store = store();
    let bound = Duration::from_secs(STALENESS_FLOOR_SECONDS.saturating_mul(3));
    let mut session = ready(&store).among(one_peer(bound.saturating_add(Duration::from_secs(1))));
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let refusal = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("no copy in reach is within the bound");

    assert!(
        matches!(refusal, Error::NoCopyWithinStaleness { .. }),
        "expected the refusal, got {refusal:?}"
    );
}

#[test]
fn a_node_that_can_answer_here_is_not_redirected_to_a_fresher_peer() {
    // *Here first*, asserted at the session because this is where it is
    // decided. A redirect this node did not need costs the client a round trip
    // and teaches it about a node it had no reason to learn — and without this
    // test, consulting the cluster BEFORE checking our own copy would pass
    // every other test in this file.
    let store = store();
    let mut session = ready(&store).among(one_peer(Duration::ZERO));

    session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect("this node is its own origin and satisfies any bound it accepts");
}

#[test]
fn a_node_told_about_no_peers_refuses_exactly_as_it_did_before() {
    // The single-node deployment, which is every deployment today. The wave
    // added a third answer and must not have moved the second one.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let refusal = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect_err("a copy with no known age is beyond every bound");

    assert!(
        matches!(refusal, Error::NoCopyWithinStaleness { .. }),
        "expected the refusal, got {refusal:?}"
    );
}

/// Level with the peer it collects from, as of now.
///
/// Goes through `Store::collected`, which is the one way this node's currency is
/// set anywhere — the collector calls it with what it observed on a link it
/// opened itself. A test seam that wrote `level_at` directly would be exactly
/// the caller the enforcement-point classification for that method warns about:
/// one that can make a stale copy look current.
/// A session on a node that is already `serving` only, so it cannot write.
///
/// `ready` defines the tenant and creates a record, which a node told
/// `DEFINE NODE ROLES serving` refuses — correctly, and that refusal is why the
/// aged test cannot simply open a second `ready`.
fn reader(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

fn level(store: &Store, reached: u64) {
    store.collected(
        tessari_types::Sequence::new(reached),
        tessari_storage::Currency::Level,
    );
}

#[test]
fn a_copy_whose_age_is_known_and_inside_the_bound_is_answered_here() {
    // The branch every other test in this file walks around. They all arrive
    // with `current_as_of()` answering `None` — a node that may not write and
    // has never collected — so the comparison between a KNOWN age and the bound
    // is reached by none of them, and the criterion's own sentence is about
    // exactly that comparison.
    //
    // The peer offered here is FRESHER than this node and is still not taken,
    // which is the *here first* rule: a redirect this node did not need costs
    // the client a round trip and hands it a node it had no reason to learn
    // about.
    let store = store();
    let mut session = ready(&store).among(one_peer(Duration::ZERO));
    session.run("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);

    let outcomes = session
        .run(&format!("SELECT * FROM orders STALENESS {};", allowed()))
        .expect("a copy level a moment ago is inside a bound three floors wide");

    assert_eq!(outcomes.len(), 1, "and it is answered here, not redirected");
}

#[test]
#[ignore = "waits out the staleness floor, which is 20 seconds by design; \
            run it with --ignored as G024 S6.2's validation"]
fn a_copy_that_has_really_aged_past_the_bound_is_routed_away_from() {
    // S6.2's validation method, in real time because the property is about real
    // time. The tightest bound the API accepts is the floor, so a copy that is
    // genuinely beyond a legal bound cannot be produced any faster than this —
    // and the alternative, a seam that backdates the currency, is the one thing
    // the currency must not have.
    //
    // One wait, both halves: past the bound, a node with a qualifying peer
    // NAMES it, and a node with no peer REFUSES rather than promoting the read
    // to the leader. Those are the two answers §C-05 and §C-07 divide between
    // them, and they differ only in what this node knows about anybody else.
    let store = store();
    let mut opening = ready(&store);
    opening.run("DEFINE NODE ROLES serving;").unwrap();
    level(&store, 1);

    // The bound is the floor itself — the tightest this API admits, so the
    // shortest honest wait. A second on top of it, because the comparison is
    // strict and a copy exactly at the bound is inside it.
    let bound = format!("{STALENESS_FLOOR_SECONDS}s");
    std::thread::sleep(Duration::from_secs(
        STALENESS_FLOOR_SECONDS.saturating_add(1),
    ));

    let mut with_a_peer = reader(&store).among(one_peer(Duration::from_secs(1)));
    let sent = with_a_peer
        .run(&format!("SELECT * FROM orders STALENESS {bound};"))
        .expect_err("this node's own copy has aged past the bound it was given");
    let Error::ReadIsElsewhere { endpoint, node, .. } = &sent else {
        panic!("a node beyond the bound answered the read itself: {sent}");
    };
    assert_eq!(endpoint, "two.example:9080");
    assert_eq!(
        *node, THERE,
        "a redirect naming only a place cannot be checked on arrival"
    );

    let mut alone = reader(&store);
    let refusal = alone
        .run(&format!("SELECT * FROM orders STALENESS {bound};"))
        .expect_err("nothing this node knows of is inside the bound");
    assert!(
        matches!(refusal, Error::NoCopyWithinStaleness { .. }),
        "a read no copy can satisfy must be refused, not promoted: {refusal:?}"
    );

    // The control that makes this about the BOUND rather than about a node that
    // has stopped answering reads: the same copy, a bound wide enough to hold
    // it, answered here.
    let generous = format!("{}s", STALENESS_FLOOR_SECONDS.saturating_mul(60));
    alone
        .run(&format!("SELECT * FROM orders STALENESS {generous};"))
        .expect("a bound wide enough for this copy is answered here");
}
