//! A subscription answers to the authority it is *currently* reading on.
//!
//! # Why this is a test and not a comment
//!
//! Because the failure it guards against has no symptom. A feed that resolved
//! its permissions once and then pushed for an hour looks exactly like a feed
//! that is entitled to: records arrive, nothing errors, and the operator who
//! ran the `REVOKE` has every reason to believe it worked — the same user's
//! `SELECT` is refused a moment later, so the half they can see agrees with
//! them.
//!
//! The test therefore holds a subscription **open across** the revocation, and
//! requires the feed itself to end. Revoking first and subscribing afterwards
//! would pass with the defect present.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::cell::Cell;

use tessaridb::feed::{Commits, Following, follow};
use tessaridb::{Db, Sequence};

const PASSWORD: &str = "correct horse battery";

/// How many rounds the feed is allowed before the test gives up on it.
///
/// A bound rather than a wait: if the revocation never reaches the loop this
/// ends the test with a failed assertion instead of hanging a suite forever.
/// Rounds are only slow when nothing is happening, and something is.
const ROUNDS_ALLOWED: u32 = 20;

#[test]
fn revoking_the_read_ends_a_subscription_that_is_already_running() {
    let db = Db::in_memory().unwrap();
    let mut owner = db.session();
    owner
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = db.session();
    root.sign_in("root", PASSWORD).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    root.run(
        "DEFINE USER kim ON NAMESPACE prod AUTHORITIES read PASSWORD 'correct horse battery';",
    )
    .unwrap();
    root.run("CREATE orders:1 = { total: 5 };").unwrap();

    let mut kim = db.session();
    kim.sign_in("kim", PASSWORD).unwrap();
    kim.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();

    let rounds = Cell::new(0_u32);
    let delivered = Cell::new(0_u32);
    let taken = Cell::new(None);
    let committed = Commits::default();
    let following = Following {
        from: Sequence::new(0),
        table: None,
    };

    let outcome = follow(
        &db,
        &mut kim,
        &following,
        &committed,
        &|| {
            rounds.set(rounds.get().saturating_add(1));
            rounds.get() > ROUNDS_ALLOWED
        },
        &mut |_change, _name, _allowed| {
            // The revocation happens *inside* the feed, on the first change it
            // pushes, so the subscription is unambiguously already running when
            // the authority goes away.
            if delivered.get() == 0 {
                let mut taking = db.session();
                taking.sign_in("root", PASSWORD).unwrap();
                taking
                    .run("USE NAMESPACE prod; USE DATABASE shop; REVOKE read ON NAMESPACE prod FROM kim;")
                    .unwrap();
                taken.set(Some(std::time::Instant::now()));
            }
            delivered.set(delivered.get().saturating_add(1));
            true
        },
    );

    assert!(delivered.get() >= 1, "the feed pushed nothing to revoke");
    let refusal = outcome.expect_err("the revocation did not reach the running feed");
    assert!(refusal.contains("read"), "{refusal}");
    assert!(
        rounds.get() <= ROUNDS_ALLOWED,
        "the feed ended because the test gave up, not because the read was revoked"
    );

    // The published bound for this point. It is one poll round, and a round is
    // at most the quarter of a second the feed blocks for when nothing is
    // happening — this measures the busy case, where the loop comes round at
    // once. Printed rather than only asserted: the number is what goes in the
    // readiness checklist.
    let bound = taken
        .get()
        .expect("the revocation was never taken")
        .elapsed();
    println!("revocation → running feed ended: {bound:?}");
    assert!(
        bound < std::time::Duration::from_secs(5),
        "one poll round took {bound:?}"
    );
}
