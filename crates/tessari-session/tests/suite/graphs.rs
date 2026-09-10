//! `DEFINE TABLE … EDGE FROM … TO …` — an edge table that names the pair it joins.
//!
//! # What the clause buys
//!
//! The bare `DEFINE TABLE follows EDGE` accepts a `RELATE` between any two
//! records at all. Adding `FROM … TO …` narrows that to one pair and refuses
//! every other, and that difference in what a caller may **do** is the whole of
//! what the clause is for. The clause is optional, so every edge table declared
//! before it existed keeps parsing and keeps accepting anything.
//!
//! # The failure this file exists for
//!
//! The predicate that gates `RELATE`, traversal, the report and the writer was
//! `kind == TableKind::Edge`, and a declared pair used to be a *different* kind —
//! so a table with endpoints answered **false** everywhere and would have been a
//! table nothing could write to and nothing could walk. None of that was a
//! compile error: the variant was new, so every `matches!` was already exhaustive
//! and every equality against `Edge` quietly kept its old answer. With the pair
//! riding **on** the edge kind rather than beside it, the state is no longer
//! representable — and the test that would have caught the regression is still
//! here, near the bottom: relate two records and walk back to them.
//!
//! # What is asserted, and what is not
//!
//! The endpoint refusal (C2) is asserted here, in both directions: the declared
//! pair is accepted and every other pair is refused, against the same store.
//! The ordered endpoint index (C3) and adjacency are later waves. Nothing here
//! asserts them, because a test that passes for a reason nobody built yet is
//! worse than a missing one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A tenancy with two ordinary tables for an edge table to join.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE COLLECTION users;\n\
             DEFINE COLLECTION posts;",
        )
        .unwrap();
    session
}

/// The report a script's last statement answered with.
fn report(session: &mut Session<'_>, script: &str) -> Value {
    let outcomes = session.run(script).unwrap();
    match outcomes.last() {
        Some(Outcome::Value(value)) => value.clone(),
        other => panic!("expected a report, got {other:?}"),
    }
}

/// The `endpoints` part of an `INFO FOR TABLE` report.
fn declared(session: &mut Session<'_>, table: &str) -> BTreeMap<String, Value> {
    let described = report(session, &format!("INFO FOR TABLE {table};"));
    let Value::Object(fields) = &described else {
        panic!("expected an object, got {described:?}");
    };
    match fields.get("endpoints") {
        Some(Value::Object(endpoints)) => endpoints.clone(),
        other => panic!("no endpoint declaration in the report: {other:?}"),
    }
}

/// The tables the database lists.
fn tables(session: &mut Session<'_>) -> Vec<String> {
    let listing = report(session, "INFO FOR DATABASE;");
    let Value::Object(fields) = &listing else {
        panic!("expected an object");
    };
    match fields.get("tables") {
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| match name {
                Value::String(text) => text.clone(),
                Value::Object(named) => match named.get("name") {
                    Some(Value::String(text)) => text.clone(),
                    _ => panic!("expected a name in {name:?}"),
                },
                other => panic!("a table name that is not text: {other:?}"),
            })
            .collect(),
        other => panic!("no table listing: {other:?}"),
    }
}

#[test]
fn an_edge_table_reports_the_pair_it_joins_in_the_direction_it_was_declared() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE wrote EDGE FROM users TO posts;")
        .unwrap();
    session
        .run("DEFINE TABLE about EDGE FROM posts TO users;")
        .unwrap();

    let wrote = declared(&mut session, "wrote");
    let about = declared(&mut session, "about");

    // The endpoints are reported as the ids the catalog holds, so the test does
    // not know their values — but it knows the two declarations name the same
    // pair in opposite orders. Asserting the swap rather than the numbers is
    // what catches the one bug this can have: resolving both endpoints and then
    // storing them the wrong way round, which no round-trip of a symmetric
    // declaration would ever notice.
    assert_eq!(wrote.get("from"), about.get("to"));
    assert_eq!(wrote.get("to"), about.get("from"));
    assert_ne!(wrote.get("from"), wrote.get("to"));
}

#[test]
fn an_edge_table_reports_the_order_its_edges_are_held_in_and_which_way_it_runs() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE recent (at datetime) EDGE FROM users TO posts ORDER BY at DESC;")
        .unwrap();
    session
        .run("DEFINE TABLE earliest (at datetime) EDGE FROM users TO posts ORDER BY at;")
        .unwrap();

    let recent = declared(&mut session, "recent");
    assert_eq!(recent.get("order"), Some(&Value::from("at")));
    assert_eq!(recent.get("descending"), Some(&Value::Bool(true)));

    // An unwritten direction is ascending, and it is *reported* rather than left
    // out — the order is the endpoint index's key suffix, so "ascending" and
    // "no order at all" describe two different keyspaces and must not read the
    // same in the report a declaration could be rebuilt from.
    let earliest = declared(&mut session, "earliest");
    assert_eq!(earliest.get("order"), Some(&Value::from("at")));
    assert_eq!(earliest.get("descending"), Some(&Value::Bool(false)));
}

#[test]
fn an_edge_table_declared_without_an_order_reports_none() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE follows EDGE FROM users TO users;")
        .unwrap();

    let follows = declared(&mut session, "follows");
    assert_eq!(follows.get("order"), None);
    assert_eq!(follows.get("descending"), None);
    // The endpoints are still there, and both name the same table.
    assert_eq!(follows.get("from"), follows.get("to"));
}

#[test]
fn an_endpoint_the_catalog_does_not_hold_is_refused_and_leaves_no_table_behind() {
    let store = store();
    let mut session = ready(&store);

    let error = session
        .run("DEFINE TABLE wrote EDGE FROM users TO nowhere;")
        .unwrap_err();
    let Error::Unknown { entity, name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "table");
    assert_eq!(name, "nowhere");

    // The endpoints resolve *before* the table is created, and this is the half
    // that matters: a table left standing with a dangling endpoint could never
    // refuse a `RELATE` against it, which is the entire capability the clause was
    // added for. A refusal that still created the table would be worse than no
    // refusal, because the store would then hold a declaration that cannot keep
    // its own promise.
    assert!(!tables(&mut session).contains(&"wrote".to_owned()));
}

#[test]
fn an_order_naming_a_field_the_table_does_not_declare_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let error = session
        .run("DEFINE TABLE recent (at datetime) EDGE FROM users TO posts ORDER BY seen DESC;")
        .unwrap_err();
    let Error::Unknown { entity, name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "field");
    assert_eq!(name, "seen");

    // The order is the endpoint index's key suffix, so it has to be readable off
    // the edge by the writer at the moment the edge is placed. An edge missing
    // the field has nowhere to be written, and the symptom would surface much
    // later, as neighbours arriving in roughly the right sequence.
    assert!(!tables(&mut session).contains(&"recent".to_owned()));
}

#[test]
fn a_declared_pair_accepts_relate_and_is_walked_the_way_a_bare_edge_table_is() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE follows EDGE FROM users TO users;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             CREATE users:3 = { handle: 'katherine' };\n\
             RELATE users:1->follows->users:2;\n\
             RELATE users:2->follows->users:3;",
        )
        .unwrap();

    // This is the regression test named in the module header. Both gates below
    // ask `is_edge()`, which a declared pair once answered false, so the first
    // `RELATE` would have been refused and the walk would have returned nothing —
    // with the catalog, the parser and every match arm compiling perfectly.
    let outcomes = session
        .run("SELECT * FROM users:1->follows->users;")
        .unwrap();
    let reached: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(reached, vec![RecordId::Int(2)]);

    let outcomes = session
        .run("SELECT * FROM users:1->follows->users->follows->users;")
        .unwrap();
    let two_hops: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(two_hops, vec![RecordId::Int(3)]);
}

#[test]
fn dropping_the_table_undefines_the_declaration_it_carries() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE follows EDGE FROM users TO users;")
        .unwrap();
    assert!(tables(&mut session).contains(&"follows".to_owned()));

    // Dropping the table takes the declaration with it, because the declaration
    // is not a second entity beside the table — it rides on the table's own kind.
    session.run("DROP TABLE follows;").unwrap();
    assert!(!tables(&mut session).contains(&"follows".to_owned()));
}

#[test]
fn a_relate_off_the_declared_pair_is_refused_in_both_of_the_ways_it_can_be_wrong() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE wrote EDGE FROM users TO posts;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             CREATE posts:1 = { title: 'notes' };",
        )
        .unwrap();

    // The pair as declared is accepted. This assertion is what stops the refusal
    // from passing by refusing everything, which is the cheap way to make the two
    // below go green.
    session.run("RELATE users:1->wrote->posts:1;").unwrap();

    // Wrong on the right-hand side: `posts` is what the table declared it leads
    // into, and `users` is not.
    let error = session.run("RELATE users:1->wrote->users:2;").unwrap_err();
    let Error::EndpointsNotDeclared { table, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(table, "wrote");

    // Right pair, wrong way round. Direction is half of what the declaration
    // says, so a check that compared the two tables as an unordered set would
    // accept this — and every walk written against `wrote` would then meet edges
    // pointing the way it does not read.
    let error = session.run("RELATE posts:1->wrote->users:1;").unwrap_err();
    assert!(
        matches!(error, Error::EndpointsNotDeclared { .. }),
        "{error}"
    );
}

#[test]
fn a_bare_edge_table_still_accepts_the_link_a_declared_one_refuses() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE wrote EDGE FROM users TO posts;\n\
             DEFINE TABLE linked EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };",
        )
        .unwrap();

    // Same store, same link, two tables: the only difference is the clause. That
    // is the whole claim the clause makes, and asserting it here is what proves
    // the refusal comes from the declaration rather than from some new rule that
    // narrowed `RELATE` for everybody — which would silently break every edge
    // table written before the clause existed.
    session.run("RELATE users:1->linked->users:2;").unwrap();
    let error = session.run("RELATE users:1->wrote->users:2;").unwrap_err();
    assert!(
        matches!(error, Error::EndpointsNotDeclared { .. }),
        "{error}"
    );
}

#[test]
fn an_edge_table_edge_is_deleted_by_its_endpoints_too() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE follows EDGE FROM users TO users;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             CREATE users:3 = { handle: 'katherine' };\n\
             RELATE users:1->follows->users:2;\n\
             RELATE users:1->follows->users:3;",
        )
        .unwrap();

    // The statement is the same one the declared-kind path takes, and it has to
    // be: the identity is derived by one rule for both, so a delete that worked
    // on only one of them would mean the rule had been written twice.
    session.run("DELETE users:1->follows->users:2;").unwrap();

    let outcomes = session
        .run("SELECT * FROM users:1->follows->users;")
        .unwrap();
    let reached: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(reached, vec![RecordId::Int(3)]);

    // The endpoint indexes go with the record, so the edge is gone from the
    // walk rather than merely from the table it lived in.
    let outcomes = session.run("SELECT * FROM users:1->follows;").unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 1);

    // And a pair the table declared it does not join is refused here as it is
    // on `RELATE`, rather than deleting nothing and reporting success.
    session
        .run("DEFINE TABLE wrote EDGE FROM users TO posts;")
        .unwrap();
    let error = session.run("DELETE users:1->wrote->users:2;").unwrap_err();
    assert!(
        matches!(error, Error::EndpointsNotDeclared { .. }),
        "{error}"
    );
}
