//! An empty node becomes a copy, and then keeps up.
//!
//! `restore.rs` proves a restored store answers what its source answered. This
//! file is about the thing that comes *after* that: the position the new node
//! must follow from. ADR-0021 states it is the applied tail plus one, derived
//! from the node's own store rather than from what the prefix claimed to hold,
//! and both halves of that sentence are load-bearing — get the first wrong and
//! the node replays or skips, get the second wrong and a prefix cut in transit
//! leaves it silently past records it never received.
//!
//! It also pins the boundary that a loose reading of "a follower is a
//! subscriber" would walk straight through: the change feed deliberately omits
//! the catalog, so it is not a replication channel. A follower moves **log
//! records**.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::Session;
use bgv_db_storage::Store;
use bgv_db_types::Sequence;

/// Enough of the store to be worth copying, and small enough to read.
///
/// Not `restore.rs`'s every-engine fixture: this file is about positions, and a
/// fixture whose job is to prove derivation would only make the sequences harder
/// to reason about.
const LEADER: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE TABLE people;\n\
DEFINE FIELD name ON people TYPE string;\n\
DEFINE INDEX by_email ON people FIELDS email UNIQUE;\n\
CREATE people:1 = { name: 'ada', email: 'a@x' };\n\
CREATE people:2 = { name: 'grace', email: 'b@x' };\n\
DELETE people:2;\n\
CREATE people:3 = { name: 'edith', email: 'c@x' };";

/// Reads that would answer differently if anything had been missed.
const INTERROGATION: &[&str] = &[
    "SELECT * FROM people;",
    "SELECT * FROM people WHERE email = 'a@x';",
    "SELECT * FROM people WHERE name > 'b';",
    "INFO FOR TABLE people;",
];

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

fn session(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
}

/// A leader holding records, and the whole prefix of its log.
fn leader() -> (Store, Vec<u8>) {
    let (_, store) = store();
    {
        let mut opening = Session::new(&store);
        opening.run(LEADER).unwrap();
    }
    let mut prefix = Vec::new();
    let written = bgv_db_backup::write(&store, &mut prefix).unwrap();
    assert!(written.records > 0);
    (store, prefix)
}

/// What both stores say to the same questions.
fn interrogate(store: &Store) -> Vec<String> {
    let mut session = session(store);
    INTERROGATION
        .iter()
        .map(|script| format!("{:?}", session.run(script).unwrap()))
        .collect()
}

#[test]
fn an_empty_node_ends_up_holding_what_the_leader_holds() {
    // Answers, not bytes. A byte comparison of two empty stores also passes, so
    // it cannot distinguish "copied everything" from "copied nothing" — which is
    // the trap criterion G2 is written against.
    let (held, prefix) = leader();
    let (_, follower) = store();

    let brought_up = bgv_db_backup::bootstrap(&follower, &mut prefix.as_slice()).unwrap();
    assert!(!brought_up.truncated);
    assert!(brought_up.records > 0, "a bootstrap that applied nothing");

    assert_eq!(interrogate(&held), interrogate(&follower));

    // And the position is the one the feed's own contract asks for: records are
    // read at or after it, so the next unseen record is one past the applied
    // tail rather than the tail itself.
    assert_eq!(
        brought_up.follow_from,
        Sequence::new(held.committed_tail().unwrap().get() + 1)
    );
}

#[test]
fn a_bootstrapped_node_follows_from_where_it_stopped_and_neither_replays_nor_skips() {
    let (held, prefix) = leader();
    let (_, follower) = store();
    let brought_up = bgv_db_backup::bootstrap(&follower, &mut prefix.as_slice()).unwrap();

    // The leader moves on, in both of the ways it can: a schema change and a
    // record change. Both must arrive, and the schema one is the half a change
    // feed would have dropped.
    {
        let mut moving = session(&held);
        moving
            .run("DEFINE FIELD city ON people TYPE string;")
            .unwrap();
        moving
            .run("CREATE people:4 = { name: 'katherine', email: 'd@x', city: 'hampton' };")
            .unwrap();
    }

    // Catching up is the same operation as bootstrapping, from a later position
    // — which is ADR-0021 §3, exercised rather than asserted.
    let mut next = Vec::new();
    let sent = bgv_db_backup::write_from(&held, &mut next, brought_up.follow_from).unwrap();
    assert!(
        sent.records > 0,
        "the leader had nothing to send, so this test proves nothing"
    );
    let caught_up = bgv_db_backup::bootstrap(&follower, &mut next.as_slice()).unwrap();

    // Nothing replayed: the records applied on the way up are exactly the ones
    // written after the bootstrap, not those plus the history again.
    assert_eq!(caught_up.records, sent.records);
    // Nothing skipped: the two stores agree again, including about the field
    // that only the catalog knows.
    assert_eq!(interrogate(&held), interrogate(&follower));
    assert_eq!(
        caught_up.follow_from,
        Sequence::new(held.committed_tail().unwrap().get() + 1)
    );
}

#[test]
fn the_change_feed_is_not_the_replication_channel() {
    // ADR-0019 §5 says a follower is a subscriber, and that is true of the
    // *buffer* — the log is durable, ordered and resumable — while being false
    // of the feed API, which deliberately omits the catalog (`feed.rs`, "The
    // catalog is not in the feed"). A follower fed from `changes_since` would
    // therefore miss every schema change, silently and forever.
    //
    // Pinned here rather than left as prose because the two are one function
    // call apart at the call site, and the wrong one type-checks.
    let (held, prefix) = leader();
    let (_, follower) = store();
    let brought_up = bgv_db_backup::bootstrap(&follower, &mut prefix.as_slice()).unwrap();

    {
        let mut moving = session(&held);
        moving
            .run("DEFINE FIELD city ON people TYPE string;")
            .unwrap();
    }

    let seen = held
        .changes_since(brought_up.follow_from, 100)
        .unwrap()
        .changes;
    assert!(
        seen.is_empty(),
        "the feed carried a catalog change, so this boundary has moved and the \
         replication path may now be reconsidered: {seen:?}"
    );
    // The log did carry it, which is what makes the omission a property of the
    // feed rather than of the commit.
    assert_eq!(
        held.log_records(brought_up.follow_from, 100).unwrap().len(),
        1
    );
}

#[test]
fn a_prefix_cut_in_transit_leaves_the_node_where_it_actually_reached() {
    // The sharp one. `Restored::tail` is what the file *claimed* to hold; a node
    // that followed from there after a truncated transfer would be positioned
    // past records it never received — a gap, silently, which is the one thing
    // replication may not do. The position must come from the node's own store.
    let (_, prefix) = leader();
    let mut cut_at_least_one = false;

    // From the end of the header onward, so the cut lands inside a length,
    // inside a sequence, inside a body, and between two records. A cut *inside*
    // the header is not a truncated prefix at all — it is not a prefix, and it
    // is refused as one in `restore.rs`.
    const HEADER_LEN: usize = 8 + 1 + 1 + (4 + 4 + 4) + 8 + 8;
    for cut in (HEADER_LEN..prefix.len()).step_by(7) {
        let (_, follower) = store();
        let brought_up = bgv_db_backup::bootstrap(&follower, &mut &prefix[..cut]).unwrap();
        assert!(brought_up.truncated, "a cut at {cut} went unnoticed");
        assert_eq!(
            brought_up.follow_from,
            Sequence::new(follower.committed_tail().unwrap().get() + 1),
            "a cut at {cut} left the node claiming a position it had not reached"
        );
        if brought_up.records > 0 {
            cut_at_least_one = true;
            // Which is strictly behind what the prefix said it held — otherwise
            // the assertion above would pass for the wrong reason.
            assert!(
                brought_up.follow_from.get() <= brought_up.records + 1,
                "a cut at {cut} followed from beyond what it applied"
            );
        }
    }
    assert!(cut_at_least_one, "no cut left a partial bootstrap to check");
}

#[test]
fn a_node_that_already_holds_something_is_refused_rather_than_merged() {
    // Bootstrapping is for an empty node. A node with a history of its own is
    // not behind the leader — it is a different store, and replaying somebody
    // else's log onto it would produce one that no log explains.
    let (_, prefix) = leader();
    let (_, occupied) = store();
    {
        let mut its_own = Session::new(&occupied);
        its_own
            .run("DEFINE NAMESPACE mine; USE NAMESPACE mine; DEFINE DATABASE theirs;")
            .unwrap();
    }

    let refused = bgv_db_backup::bootstrap(&occupied, &mut prefix.as_slice()).unwrap_err();
    assert!(
        matches!(refused, bgv_db_backup::Error::WrongBase { .. }),
        "refused for the wrong reason: {refused}"
    );
}
