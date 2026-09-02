//! What an `ORDER BY` key is allowed to see.
//!
//! Two rules that pull in opposite directions, and the store needs both:
//!
//! - a key may name **what the answer carries** — `SELECT address.city AS home …
//!   ORDER BY home` — because that is the name the caller is looking at;
//! - a key may name **what the record holds** — `SELECT name … ORDER BY
//!   geo::distance(shape, $here)` — because a bounded nearest-first read is the
//!   whole point of ordering, and it rarely wants to project the field it
//!   measures.
//!
//! Before wave 101 only the first held, and the second failed **silently**: the
//! key evaluated to an absence on every record, every record tied, and the read
//! answered in whatever order the source produced. No error, plausible rows, and
//! nothing in the suite asserting either way.
//!
//! The shadowing case below is the one that fails if the fix is applied the
//! wrong way round.

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
             DEFINE COLLECTION items;",
        )
        .unwrap();
    session
}

/// The value of one field of every record a statement answered with, in the
/// order the answer put them.
fn column(session: &mut Session<'_>, script: &str, field: &str) -> Vec<Value> {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    records
        .iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("a record is an object")
            };
            fields
                .get(field)
                .cloned()
                .unwrap_or_else(|| panic!("every answer should carry `{field}`, got {fields:?}"))
        })
        .collect()
}

fn text(value: &str) -> Value {
    Value::from(value)
}

/// Three items whose `label` and whose `rank` disagree about the order, so a
/// read that ordered by the wrong one is visible rather than coincidental.
fn three_items(session: &mut Session<'_>) {
    session
        .run(
            "CREATE items:1 = { label: 'alpha', rank: 3 };\n\
             CREATE items:2 = { label: 'beta',  rank: 1 };\n\
             CREATE items:3 = { label: 'gamma', rank: 2 };",
        )
        .unwrap();
}

#[test]
fn a_key_may_name_a_field_the_projection_dropped() {
    let store = store();
    let mut session = ready(&store);
    three_items(&mut session);

    // `rank` is not projected. Before this was fixed, every record's key was an
    // absence, they all tied, and the answer came back in insertion order —
    // which is `alpha, beta, gamma`, and would have looked entirely reasonable.
    assert_eq!(
        column(
            &mut session,
            "SELECT label FROM items ORDER BY rank;",
            "label"
        ),
        vec![text("beta"), text("gamma"), text("alpha")]
    );
}

#[test]
fn a_key_may_still_name_the_alias_the_answer_carries() {
    let store = store();
    let mut session = ready(&store);
    three_items(&mut session);

    assert_eq!(
        column(
            &mut session,
            "SELECT rank AS position FROM items ORDER BY position;",
            "position"
        ),
        vec![Value::from(1_i64), Value::from(2_i64), Value::from(3_i64)]
    );
}

#[test]
fn an_alias_wins_over_the_source_field_whose_name_it_takes() {
    // The answer carries `label`, but it holds the *rank*; ordering by `label`
    // must read what the answer carries, which puts them in rank order — not in
    // alphabetical order, which is what the shadowed source field would give.
    let store = store();
    let mut session = ready(&store);
    three_items(&mut session);

    assert_eq!(
        column(
            &mut session,
            "SELECT rank AS label FROM items ORDER BY label;",
            "label"
        ),
        vec![Value::from(1_i64), Value::from(2_i64), Value::from(3_i64)]
    );
}

#[test]
fn an_alias_still_wins_when_a_second_key_forces_the_source_into_view() {
    // The case that fails if the overlay is written the wrong way round, and
    // the *only* one that can: a statement that shadows a name without also
    // naming a dropped field never builds an overlay at all, so it cannot tell
    // the two directions apart. Here `hidden` is dropped, so the overlay is
    // built — and `label` must still mean the alias rather than the source field
    // the overlay has just brought back into scope.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE items:1 = { label: 'alpha', tag: 'A', rank: 3, hidden: 1 };\n\
             CREATE items:2 = { label: 'beta',  tag: 'B', rank: 1, hidden: 1 };\n\
             CREATE items:3 = { label: 'gamma', tag: 'C', rank: 1, hidden: 0 };",
        )
        .unwrap();

    // Alias wins  → order by rank, then hidden → C, B, A.
    // Source wins → order by the text label, then hidden → A, B, C.
    assert_eq!(
        column(
            &mut session,
            "SELECT rank AS label, tag FROM items ORDER BY label, hidden;",
            "tag"
        ),
        vec![text("C"), text("B"), text("A")]
    );
}

#[test]
fn a_bounded_read_over_a_dropped_field_keeps_the_records_the_order_puts_first() {
    // A bound cannot be allowed to interact with the fix: the record that
    // belongs first may be the last one the source produces, and the overlay is
    // built before the bound is offered anything.
    let store = store();
    let mut session = ready(&store);
    three_items(&mut session);

    assert_eq!(
        column(
            &mut session,
            "SELECT label FROM items ORDER BY rank LIMIT 2;",
            "label"
        ),
        vec![text("beta"), text("gamma")]
    );
}

#[test]
fn descending_over_a_dropped_field_reverses_the_same_order() {
    let store = store();
    let mut session = ready(&store);
    three_items(&mut session);

    assert_eq!(
        column(
            &mut session,
            "SELECT label FROM items ORDER BY rank DESC;",
            "label"
        ),
        vec![text("alpha"), text("gamma"), text("beta")]
    );
}

#[test]
fn a_computed_key_over_a_dropped_field_orders_by_what_it_computes() {
    // The shape the defect was found in: the key is a call, not a bare path, so
    // the field it reads is one level down in the expression tree. A walk that
    // only looked at top-level paths would pass every other test here and fail
    // this one.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE items:1 = { label: 'alpha', code: 'zzz' };\n\
             CREATE items:2 = { label: 'beta',  code: 'a' };\n\
             CREATE items:3 = { label: 'gamma', code: 'mm' };",
        )
        .unwrap();

    assert_eq!(
        column(
            &mut session,
            "SELECT label FROM items ORDER BY string::len(code);",
            "label"
        ),
        vec![text("beta"), text("gamma"), text("alpha")]
    );
}

#[test]
fn two_keys_are_read_from_wherever_each_of_them_lives() {
    // One key names the alias, the other names a dropped source field. The
    // overlay has to serve both in one evaluation.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE items:1 = { label: 'alpha', group: 2, rank: 1 };\n\
             CREATE items:2 = { label: 'beta',  group: 1, rank: 2 };\n\
             CREATE items:3 = { label: 'gamma', group: 1, rank: 1 };",
        )
        .unwrap();

    assert_eq!(
        column(
            &mut session,
            "SELECT group AS band, label FROM items ORDER BY band, rank;",
            "label"
        ),
        vec![text("gamma"), text("beta"), text("alpha")]
    );
}
