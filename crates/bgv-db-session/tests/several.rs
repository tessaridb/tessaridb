//! `[*]` — a route that reaches several values.
//!
//! The step that turns a path from a **function** into a **relation**. What a
//! context does with several values is the context's own rule, and only one of
//! the three contexts is built: a comparison holds when **any** reached value
//! satisfies it. A projection over several and an index over several are their
//! own tasks, and are refused by name here rather than half-built — so most of
//! this file is about where `[*]` may *not* stand, which is the part that would
//! otherwise be discovered by somebody getting a wrong answer.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{AccessPath, Error, Session};
use bgv_db_storage::Store;
use bgv_db_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Notes with tags, and a few shapes that are deliberately not arrays.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE work; USE DATABASE work;\n\
             DEFINE TABLE notes;\n\
             CREATE notes:1 = { title: 'first', tags: ['urgent', 'draft'] };\n\
             CREATE notes:2 = { title: 'second', tags: ['draft'] };\n\
             CREATE notes:3 = { title: 'third', tags: [] };\n\
             CREATE notes:4 = { title: 'fourth' };\n\
             CREATE notes:5 = { title: 'fifth', tags: 'urgent' };\n\
             CREATE notes:6 = { title: 'sixth', \
                                items: [{ sku: 'a1', n: 2 }, { sku: 'b2', n: 5 }] };",
        )
        .unwrap();
    session
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

#[test]
fn a_comparison_over_several_holds_when_any_of_them_does() {
    // The whole feature. `notes:1` has `urgent` among its tags; `notes:2` does
    // not; `notes:5` holds `urgent` as a single value and is deliberately not a
    // match — see below.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';"
        ),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn a_single_value_is_not_an_array_of_one() {
    // `notes:5` holds `tags: 'urgent'` — a string, not an array. `tags[*]`
    // reaches nothing there, the same way `name CONTAINS 'ada'` is not
    // `name = 'ada'`: a mistake in a query should show as no match rather than
    // as a right-looking answer.
    let store = store();
    let mut session = ready(&store);
    let found = ids(
        &mut session,
        "SELECT * FROM notes WHERE tags[*] = 'urgent';",
    );
    assert!(
        !found.contains(&RecordId::Int(5)),
        "a single value answered as an array of one: {found:?}"
    );
    // …and asking about it as the value it is still works.
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags = 'urgent';"),
        vec![RecordId::Int(5)]
    );
}

#[test]
fn an_empty_array_and_an_absent_field_match_nothing_and_fail_nothing() {
    let store = store();
    let mut session = ready(&store);
    let found = ids(
        &mut session,
        "SELECT * FROM notes WHERE tags[*] = 'anything';",
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn it_reaches_through_an_array_of_objects() {
    // `items[*].sku` is the relation composed with a field: every element, then
    // that element's `sku`. This is the shape a document engine is for.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE items[*].sku = 'b2';"
        ),
        vec![RecordId::Int(6)]
    );
    assert!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE items[*].sku = 'nope';"
        )
        .is_empty()
    );
}

#[test]
fn an_ordered_comparison_over_several_is_any_of_them_too() {
    // Not only equality: `items[*].n > 4` asks whether any element's `n` is.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE items[*].n > 4;"),
        vec![RecordId::Int(6)]
    );
    assert!(
        ids(&mut session, "SELECT * FROM notes WHERE items[*].n > 9;").is_empty(),
        "an ordered comparison matched where no element does"
    );
}

#[test]
fn negating_any_of_them_means_none_of_them() {
    // `NOT tags[*] = 'urgent'` is the negation of "any of them is urgent",
    // which is "none of them is" — and that includes the records with no tags
    // at all, because none of nothing is urgent.
    let store = store();
    let mut session = ready(&store);
    let found = ids(
        &mut session,
        "SELECT * FROM notes WHERE NOT tags[*] = 'urgent';",
    );
    assert!(!found.contains(&RecordId::Int(1)), "{found:?}");
    assert!(found.contains(&RecordId::Int(2)), "{found:?}");
    assert!(found.contains(&RecordId::Int(3)), "{found:?}");
    assert!(found.contains(&RecordId::Int(4)), "{found:?}");
}

#[test]
fn it_composes_with_the_rest_of_a_condition() {
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'draft' AND title = 'second';"
        ),
        vec![RecordId::Int(2)]
    );
}

#[test]
fn no_index_serves_a_condition_over_several() {
    // The rule this store will not bend: an index changes what a read costs and
    // never what it answers. An ordinary index over `tags` holds **one** entry
    // for the whole array, so serving `tags[*] = 'urgent'` from it would answer
    // a question about elements with an answer about arrays. Until there is a
    // multikey index, the read is a scan — and the access path says so rather
    // than the store hoping nobody asks.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_tags ON notes FIELDS tags;")
        .unwrap();

    let outcomes = session
        .run("SELECT * FROM notes WHERE tags[*] = 'urgent';")
        .unwrap();
    assert_eq!(outcomes[0].path().unwrap(), AccessPath::Scan);
    // And the answer is the one the scan gives, index or no index.
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';"
        ),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn every_position_without_a_rule_yet_is_refused_by_name() {
    // Half-building the other two contexts would be worse than refusing them:
    // a projection over several would have to invent what an empty reach
    // answers, and an index over several would answer about arrays. Each
    // refusal names `[*]` rather than reporting a stray token.
    let store = store();
    let mut session = ready(&store);
    for script in [
        // A projection: SGJ.T2.
        "SELECT tags[*] AS all_tags FROM notes;",
        // An index: SGJ.T3.
        "DEFINE INDEX by_tag ON notes FIELDS tags[*];",
        // A key and an ordering: both are per record and neither has a rule.
        "SELECT tags[*] AS t, count(*) AS n FROM notes GROUP BY tags[*];",
        "SELECT * FROM notes ORDER BY tags[*];",
        // A function argument.
        "SELECT * FROM notes WHERE array::len(tags[*]) = 2;",
        // The right-hand side of a comparison — the same question backwards,
        // and a second spelling for one thing.
        "SELECT * FROM notes WHERE 'urgent' = tags[*];",
        // A `FETCH` route.
        "SELECT * FROM notes FETCH tags[*];",
    ] {
        let refused = session.run(script);
        assert!(
            matches!(refused, Err(Error::Script(_))),
            "{script} was accepted: {refused:?}"
        );
    }
}

#[test]
fn a_route_without_it_behaves_exactly_as_it_did() {
    // The floor. A position still addresses one element, and a plain field is
    // still a plain field.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[0] = 'urgent';"
        ),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE title = 'third';"),
        vec![RecordId::Int(3)]
    );
}
