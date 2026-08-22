//! `[*]` — a route that reaches several values.
//!
//! The step that turns a path from a **function** into a **relation**. What a
//! context does with several values is the context's own rule, and two of the
//! three contexts are built: a comparison holds when **any** reached value
//! satisfies it, and a projection answers with **all** of them. An index over
//! several is its own task and is refused by name here rather than half-built —
//! so a good part of this file is about where `[*]` may *not* stand, which is
//! the part that would otherwise be discovered by somebody getting a wrong
//! answer.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{AccessPath, Error, Session};
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

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
                                items: [{ sku: 'a1', n: 2 }, { sku: 'b2', n: 5 }] };\n\
             CREATE notes:7 = { title: 'seventh', tags: ['dup', 'dup'] };",
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
    // Half-building the remaining context would be worse than refusing it: an
    // index over several would answer a question about elements with an answer
    // about arrays. The same holds for the positions that are not a context at
    // all — a key, an ordering, a function argument. Each refusal names `[*]`
    // rather than reporting a stray token.
    let store = store();
    let mut session = ready(&store);
    for script in [
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

/// One projected field of the one answer a read gives, as text.
fn projected(session: &mut Session<'_>, script: &str, field: &str) -> String {
    let outcomes = session.run(script).unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1, "expected one record: {records:?}");
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object: {:?}", records[0].1);
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

#[test]
fn a_projection_answers_with_all_of_them() {
    // The second of the three contexts. A comparison over several tests; a
    // projection **collects**.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        projected(
            &mut session,
            "SELECT tags[*] AS all_tags FROM notes:1;",
            "all_tags"
        ),
        r#"Array([String("urgent"), String("draft")])"#
    );
}

#[test]
fn it_collects_rather_than_summarising() {
    // Route order, duplicates kept. A projection reports what is there:
    // deduplicating or sorting would be a different statement, and one the
    // caller did not write.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        projected(
            &mut session,
            "SELECT tags[*] AS all_tags FROM notes:7;",
            "all_tags"
        ),
        r#"Array([String("dup"), String("dup")])"#
    );
}

#[test]
fn a_projection_reaches_through_an_array_of_objects() {
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        projected(
            &mut session,
            "SELECT items[*].sku AS skus FROM notes:6;",
            "skus"
        ),
        r#"Array([String("a1"), String("b2")])"#
    );
}

#[test]
fn three_records_that_reach_nothing_answer_the_same_empty_array() {
    // The one real question this task had to settle, and the answer is that
    // there was nothing to settle once the denotation was taken seriously. An
    // empty array, an absent field and a single value all **reach nothing**, so
    // they answer the same thing — a projection that told them apart would be
    // reading whether the field exists, which `tags` already answers on its own.
    //
    // And that thing is `[]` rather than an omitted field, because a relation is
    // **total**: every record has a reach, and zero of them is an empty
    // collection rather than an absence. The store's older rule — a projection
    // that reaches nothing omits its field — is about an expression having *no
    // value*, and this expression has one.
    let store = store();
    let mut session = ready(&store);
    for id in ["notes:3", "notes:4", "notes:5"] {
        assert_eq!(
            projected(
                &mut session,
                &format!("SELECT tags[*] AS all_tags FROM {id};"),
                "all_tags"
            ),
            "Array([])",
            "{id} did not answer with an empty array"
        );
    }
    // …while the field itself still says what it is, which is the question the
    // projection above is deliberately not answering.
    assert_eq!(
        projected(&mut session, "SELECT tags FROM notes:3;", "tags"),
        "Array([])"
    );
    assert_eq!(
        projected(&mut session, "SELECT tags FROM notes:4;", "tags"),
        "None"
    );
    assert_eq!(
        projected(&mut session, "SELECT tags FROM notes:5;", "tags"),
        r#"String("urgent")"#
    );
}

#[test]
fn a_multi_valued_projection_still_has_to_be_named() {
    // `tags[*]` has no name of its own: every invented spelling — `tags_0`,
    // `tags`, `_0` — is a convention the author would learn from a surprise.
    let store = store();
    let mut session = ready(&store);
    let refused = session.run("SELECT tags[*] FROM notes;");
    assert!(
        matches!(refused, Err(Error::Script(_))),
        "an unnamed multi-valued projection was accepted: {refused:?}"
    );
}

#[test]
fn a_projection_admits_it_whole_and_not_inside_something_larger() {
    // The rule is one per context, and this is where the projection's ends.
    // `array::len(tags[*])` has two defensible answers — the function over the
    // collected values, or the function applied to each of them — and a language
    // that picks one silently teaches the other by surprise.
    let store = store();
    let mut session = ready(&store);
    for script in [
        "SELECT array::len(tags[*]) AS n FROM notes;",
        "SELECT string::upper(tags[*]) AS shout FROM notes;",
        "SELECT tags[*] + 'x' AS odd FROM notes;",
        "SELECT [tags[*]] AS wrapped FROM notes;",
    ] {
        let refused = session.run(script);
        assert!(
            matches!(refused, Err(Error::Script(_))),
            "{script} was accepted: {refused:?}"
        );
    }
}

#[test]
fn a_grouped_read_still_refuses_it() {
    // It is neither a group key nor a fold, so it has as many values as the
    // group has records — the rule that keeps a wrong number out of a report,
    // and it needed nothing new to keep holding.
    let store = store();
    let mut session = ready(&store);
    let refused =
        session.run("SELECT title, tags[*] AS t, count(*) AS n FROM notes GROUP BY title;");
    assert!(
        matches!(refused, Err(Error::Script(_))),
        "a multi-valued projection in a grouped read was accepted: {refused:?}"
    );
}

#[test]
fn an_ordering_may_name_what_the_projection_produced() {
    // A projected array is an ordinary value, so ordering by the name it answers
    // under needs nothing built. `notes:3` reaches nothing and sorts first;
    // `notes:7`'s two `dup`s sort last.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT title, tags[*] AS t FROM notes ORDER BY t, title;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    let first = &records[0].1;
    let last = &records[records.len() - 1].1;
    let held = |value: &Value| {
        let Value::Object(fields) = value else {
            panic!("not an object: {value:?}");
        };
        format!("{:?}", fields.get("t").unwrap_or(&Value::None))
    };
    assert_eq!(held(first), "Array([])", "{records:?}");
    assert_eq!(
        held(last),
        r#"Array([String("urgent"), String("draft")])"#,
        "{records:?}"
    );
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
