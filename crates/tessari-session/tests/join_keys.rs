//! A join refuses a key-type mismatch instead of answering with no rows.
//!
//! `no rows` is the honest answer to a join over data that happens not to
//! match. It is also what a join answers when one side stores an identity as
//! text and the other stores it as a reference — the single commonest way a
//! join is written wrong. The two answers are byte-for-byte the same, and a
//! reader who gets the second one has no way to tell it from the first.
//!
//! So the store separates them. When the answer is empty **and** both sides
//! held something at the key **and** their kinds share nothing, the read fails
//! and says which two kinds it compared.
//!
//! The two tests that matter most here are the negative ones: a join that
//! legitimately matches nothing must still answer `[]`, and a join that matched
//! anything at all must never be refused. A safety rule that fires on correct
//! queries is worse than the hole it closes.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users; DEFINE TABLE orders;",
        )
        .unwrap();
    session
}

/// The rows one read answered with.
fn rows(session: &mut Session<'_>, script: &str) -> Vec<(tessari_types::RecordId, Value)> {
    match session.run(script).unwrap().last().unwrap() {
        Outcome::Records { records, .. } => records.clone(),
        other => panic!("not records: {other:?}"),
    }
}

/// `orders.who` holds a reference; `users.tag` holds the same identity as text.
fn mismatched(session: &mut Session<'_>) {
    session
        .run(
            "CREATE users:1 = { tag: 'users:1' };\n\
             CREATE users:2 = { tag: 'users:2' };\n\
             CREATE orders:1 = { who: users:1, total: 3 };\n\
             CREATE orders:2 = { who: users:2, total: 7 };",
        )
        .unwrap();
}

const ACROSS_KINDS: &str = "SELECT * FROM users JOIN orders ON users.tag = orders.who;";

#[test]
fn a_reference_against_text_is_refused_rather_than_answered() {
    let store = store();
    let mut session = ready(&store);
    mismatched(&mut session);

    let refused = session.run(ACROSS_KINDS).expect_err("a refusal");
    let said = refused.to_string();
    assert!(said.contains("string"), "{said}");
    assert!(said.contains("record"), "{said}");
}

#[test]
fn the_refusal_names_both_routes() {
    // The message has to be actionable from the statement alone: which two
    // fields, and what each of them turned out to hold.
    let store = store();
    let mut session = ready(&store);
    mismatched(&mut session);

    let said = session
        .run(ACROSS_KINDS)
        .expect_err("a refusal")
        .to_string();
    assert!(said.contains("tag"), "{said}");
    assert!(said.contains("who"), "{said}");
}

#[test]
fn the_index_served_path_refuses_too() {
    // The path that would otherwise never learn what the right side holds: it
    // probes an index and is told nothing, which is indistinguishable from a
    // key that is simply absent. This is the arm the rule exists for, since it
    // is the one a production schema takes.
    let store = store();
    let mut session = ready(&store);
    mismatched(&mut session);
    session
        .run("DEFINE INDEX by_who ON orders FIELDS who;")
        .unwrap();

    assert!(
        session.run(ACROSS_KINDS).is_err(),
        "an index turned the refusal back into an empty answer"
    );
}

#[test]
fn a_join_that_legitimately_matches_nothing_still_answers() {
    // The rule must not fire here. Both sides hold strings; they simply share no
    // value, which is an ordinary empty answer and not a mistake.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE users:1 = { name: 'ada' };\n\
             CREATE orders:1 = { who: 'nobody', total: 3 };",
        )
        .unwrap();

    let found = rows(
        &mut session,
        "SELECT * FROM users JOIN orders ON users.name = orders.who;",
    );
    assert!(found.is_empty());
}

#[test]
fn a_join_that_matched_anything_is_never_refused() {
    // One good pair among mismatched ones is enough. Records carry no declared
    // type here, so refusing on a single stray value would trade a silent wrong
    // answer for a loud wrong refusal.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE users:1 = { tag: 'ada' };\n\
             CREATE users:2 = { tag: users:2 };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };",
        )
        .unwrap();

    let found = rows(&mut session, ACROSS_KINDS);
    assert_eq!(found.len(), 1, "the matching pair was lost");
}

#[test]
fn an_empty_side_is_not_a_type_mistake() {
    // Nothing to reconcile: a join over a table holding no records answers
    // nothing for the ordinary reason, and saying otherwise would refuse every
    // query written before its data arrives.
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE users:1 = { tag: 'ada' };").unwrap();

    assert!(rows(&mut session, ACROSS_KINDS).is_empty());
}

#[test]
fn a_where_that_discards_every_row_is_not_a_type_mistake() {
    // The keys matched and the condition then rejected the rows, so the answer
    // is empty for a reason the reader wrote themselves. The kinds overlap, so
    // the rule cannot fire — asserted because the check runs after the
    // condition and would be wrong if it read only the row count.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE users:1 = { name: 'ada' };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };",
        )
        .unwrap();

    let found = rows(
        &mut session,
        "SELECT * FROM users JOIN orders ON users.name = orders.who WHERE orders.total > 100;",
    );
    assert!(found.is_empty());
}

#[test]
fn a_key_absent_on_one_side_is_not_a_type_mistake() {
    // A left record with nothing at the key contributes no kind at all, so a
    // join whose left side never carries the field is empty rather than
    // refused. The field is simply not there, which is a different sentence
    // from "it is there and holds the wrong sort of thing".
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE users:1 = { name: 'ada' };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };",
        )
        .unwrap();

    let found = rows(
        &mut session,
        "SELECT * FROM users JOIN orders ON users.missing = orders.who;",
    );
    assert!(found.is_empty());
}
