//! An identity the store produced is made once and carried, never re-derived.
//!
//! `INSERT` asks the store for a record identity when the caller supplies none.
//! That is a *decision*, and a decision made twice is two decisions. If the log
//! carried the statement rather than what the statement wrote, every node
//! applying it would produce its own identities, and the store would then hold
//! the same records under different names on every replica — while each node,
//! read alone, looked entirely correct. Nothing in a single-node test can see
//! that; it is visible only where a second node exists.
//!
//! # Why two followers rather than one
//!
//! A follower whose identities match the leader's proves the identity reached
//! it. It does *not* prove the identity was not re-derived, because a
//! re-derivation that happens to be deterministic — seeded from the sequence
//! number, say — would match too, and would then diverge the first time two
//! nodes applied at different times or in different orders.
//!
//! Two followers built from the *same* log settle it. If either node decides
//! anything about identity, they disagree; if the identity is carried, they
//! cannot.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::RecordId;

const TENANCY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE COLLECTION readings;";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The identities a store holds in `readings`, in the order it reads them back.
fn held(store: &Store) -> Vec<RecordId> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
        .run("SELECT * FROM readings;")
        .unwrap()
        .pop()
        .expect("one outcome")
        .records()
        .expect("records")
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

/// A leader that ran one `INSERT`, the identities it answered with, and the
/// whole prefix of its log.
fn leader() -> (Store, Vec<RecordId>, Vec<u8>) {
    let store = store();
    let answered = {
        let mut session = Session::new(&store);
        session.run(TENANCY).unwrap();
        session
            .run("INSERT INTO readings (n) VALUES (1), (2), (3);")
            .unwrap()
            .pop()
            .expect("one outcome")
            .keys()
            .expect("keys")
            .to_vec()
    };
    assert_eq!(answered.len(), 3);

    let mut prefix = Vec::new();
    let written = tessari_backup::write(&store, &mut prefix).unwrap();
    assert!(written.records > 0, "a log that carried nothing");
    (store, answered, prefix)
}

fn follower_of(prefix: &[u8]) -> Store {
    let store = store();
    let brought_up = tessari_backup::bootstrap(&store, &mut { prefix }).unwrap();
    assert!(brought_up.records > 0, "a bootstrap that applied nothing");
    store
}

#[test]
fn a_second_node_holds_the_identities_the_first_one_produced() {
    let (leader, answered, prefix) = leader();

    // The leader's own answer describes the leader's own store. Asserting that
    // first means a later failure is about replication and not about `INSERT`.
    let mut on_the_leader = held(&leader);
    let mut expected = answered;
    on_the_leader.sort();
    expected.sort();
    assert_eq!(
        on_the_leader, expected,
        "the leader answered with identities it does not hold"
    );

    let mut on_the_follower = held(&follower_of(&prefix));
    on_the_follower.sort();
    assert_eq!(
        on_the_follower, expected,
        "a node applying the log produced its own identities instead of the ones in it"
    );
}

#[test]
fn two_nodes_applying_one_log_agree_on_every_identity() {
    let (_leader, _answered, prefix) = leader();

    let mut first = held(&follower_of(&prefix));
    let mut second = held(&follower_of(&prefix));
    first.sort();
    second.sort();

    assert_eq!(
        first, second,
        "two nodes applying the same log disagreed about identity, so something \
         downstream of the leader is still deciding it"
    );
}
