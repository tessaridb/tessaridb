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

use tessari_constants::STALENESS_FLOOR_SECONDS;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;

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
