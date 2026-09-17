//! `ANSWERED BY` — the second axis a read may name, and the one a freshness
//! bound cannot express.
//!
//! # What these cases are actually protecting
//!
//! A follower at zero lag is level, not authoritative: being level a moment ago
//! says nothing about a write committing right now. So a read that must come
//! from where writes are decided cannot be written as a tight `STALENESS`, and
//! the failure of trying is the quiet kind — the follower answers, the caller
//! gets records, and nothing anywhere is in an error state.
//!
//! The cases below are therefore mostly about **refusals and redirects**, which
//! are the only observable difference between this clause working and this
//! clause being decorative.
//!
//! The peer is hand-written for `staleness.rs`'s reason: `tessari-wire` depends
//! on this crate and not the reverse, so a real `Directory` is not nameable
//! here. What this file owns is the SESSION's half — that it asks the right
//! question, in the right order, and uses the answer. That the directory picks
//! the right peer is asserted where the directory lives.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use core::time::Duration;

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

const THERE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];
const THEIR_EPOCH: Epoch = Epoch::new(7);

/// A cluster of one peer, which may or may not claim it writes.
#[derive(Debug)]
struct OnePeer {
    endpoint: String,
    leads: bool,
}

impl Elsewhere for OnePeer {
    fn within(&self, _bound: Duration) -> Option<Peer> {
        None
    }

    fn writable(&self) -> Option<Peer> {
        self.leads.then(|| Peer {
            endpoint: self.endpoint.clone(),
            node: THERE,
            epoch: THEIR_EPOCH,
        })
    }
}

fn peer(leads: bool) -> Arc<dyn Elsewhere> {
    Arc::new(OnePeer {
        endpoint: "two.example:9080".to_owned(),
        leads,
    })
}

#[test]
fn a_node_that_leads_answers_the_read_itself() {
    // A fresh store's own node writes, so it is the leader of everything it
    // holds and the clause is satisfied here. This is the case that would pass
    // whatever the clause did, which is exactly why it is not the only one.
    let store = store();
    let mut session = ready(&store);

    let outcomes = session
        .run("SELECT * FROM orders ANSWERED BY LEADER;")
        .expect("a node that may write is the leader of its own records");

    assert_eq!(outcomes.len(), 1);
}

#[test]
fn any_copy_is_the_default_and_writing_it_changes_nothing() {
    let store = store();
    let mut session = ready(&store);

    let written = session
        .run("SELECT * FROM orders ANSWERED BY ANY;")
        .expect("any copy includes this one");
    let unwritten = session
        .run("SELECT * FROM orders;")
        .expect("the default is the same thing unsaid");

    assert_eq!(written.len(), unwritten.len());
}

#[test]
fn a_follower_asked_for_the_leader_redirects_to_the_peer_that_claims_it() {
    let store = store();
    let mut session = ready(&store).among(peer(true));
    // Drained of the writable role, this node is a follower — which is the only
    // state in which the clause has anything to do.
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let sent = session
        .run("SELECT * FROM orders ANSWERED BY LEADER;")
        .expect_err("a redirect is an answer, and it arrives as one of these");

    let Error::ReadIsElsewhere {
        because,
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
    assert_eq!(
        *epoch, THEIR_EPOCH,
        "the epoch is the NAMED peer's own claim, never this node's"
    );
    assert!(
        because.contains("ANSWERED BY LEADER"),
        "the redirect has to say which of the two axes sent the client away: {because}"
    );
}

#[test]
fn a_follower_that_knows_of_no_leader_refuses_and_names_the_remedy() {
    let store = store();
    let mut session = ready(&store).among(peer(false));
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let refusal = session
        .run("SELECT * FROM orders ANSWERED BY LEADER;")
        .unwrap_err();

    assert!(
        matches!(refusal, Error::NoLeaderKnown { .. }),
        "a cluster with no writable member is its own class, not a staleness \
         refusal wearing a different message: {refusal}"
    );
    assert!(
        refusal.to_string().contains("ROLES writable"),
        "a refusal a caller cannot act on is an obstruction: {refusal}"
    );
}

#[test]
fn a_follower_with_no_peers_at_all_refuses_rather_than_answering_locally() {
    // The dangerous direction. Nobody told this node about anybody, and the
    // read said where it had to come from — answering it here would be the
    // quiet wrong answer the whole clause exists to prevent.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let refusal = session
        .run("SELECT * FROM orders ANSWERED BY LEADER;")
        .unwrap_err();

    assert!(matches!(refusal, Error::NoLeaderKnown { .. }), "{refusal}");
}

#[test]
fn the_two_axes_compose_and_authority_is_decided_first() {
    // The peer leads but is outside every bound this stub admits, and the read
    // names both axes. Authority is decided first, so the answer is the
    // redirect to the leader — not the staleness refusal.
    //
    // The order matters because it is the one that cannot contradict itself: a
    // node holding the writable role is level with itself, so the leader
    // satisfies every bound; deciding freshness first could pick a follower
    // inside the bound and answer a read that said it had to come from the
    // leader.
    let store = store();
    let mut session = ready(&store).among(peer(true));
    session.run("DEFINE NODE ROLES serving;").unwrap();

    let sent = session
        .run(&format!(
            "SELECT * FROM orders STALENESS {}s ANSWERED BY LEADER;",
            tessari_constants::STALENESS_FLOOR_SECONDS.saturating_mul(3)
        ))
        .unwrap_err();

    let Error::ReadIsElsewhere { because, .. } = &sent else {
        panic!("the freshness axis overtook the authority axis: {sent}");
    };
    assert!(
        because.contains("ANSWERED BY LEADER"),
        "authority was decided second: {because}"
    );
}

#[test]
fn an_unknown_answerer_is_refused_where_the_statement_is_read() {
    // The refusal that makes the clause safe. Somebody who wrote `MASTER` meant
    // the leader; a parser that shrugged and admitted any copy would answer the
    // read they were careful about from a follower.
    let store = store();
    let mut session = ready(&store);

    let refusal = session
        .run("SELECT * FROM orders ANSWERED BY MASTER;")
        .unwrap_err();

    let message = refusal.to_string();
    assert!(
        message.contains("MASTER"),
        "a refusal that does not echo what was written cannot be acted on: {message}"
    );
    assert!(
        message.contains("ANSWERED BY LEADER"),
        "and it has to name what would have been accepted: {message}"
    );
}

#[test]
fn answered_by_with_no_node_is_refused_rather_than_ignored() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("SELECT * FROM orders ANSWERED BY;")
        .expect_err("a clause opened and left unfinished is a mistake in the statement");
}

#[test]
fn a_field_called_answered_is_still_a_field() {
    // The contextual guard. Two words are required before the clause opens, so
    // a name called `answered` reads as a name everywhere else.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE tickets SCHEMALESS;\n\
             CREATE tickets:1 = { answered: true };",
        )
        .unwrap();

    session
        .run("SELECT answered FROM tickets;")
        .expect("a word that opens a clause only in a clause's position");
}

#[test]
fn a_read_answered_by_the_leader_is_not_promised_to_be_repeatable() {
    // The limit that ships with the clause, asserted rather than only written
    // down. Two authoritative reads with a write in between legitimately
    // differ, and neither is wrong: the clause says where the answer comes
    // from, not that the store holds still while the caller reads.
    let store = store();
    let mut session = ready(&store);

    let first = session
        .run("SELECT * FROM orders ANSWERED BY LEADER;")
        .unwrap();
    session.run("CREATE orders:2 = { total: 20 };").unwrap();
    let second = session
        .run("SELECT * FROM orders ANSWERED BY LEADER;")
        .unwrap();

    assert_ne!(
        format!("{first:?}"),
        format!("{second:?}"),
        "if these agreed, the clause would be promising a stability it does not \
         have, and this test would be asserting the wrong contract"
    );
}
