//! `DEFINE GRAPH` — an edge table that says which pair of tables it joins.
//!
//! # What the word buys
//!
//! `DEFINE TABLE follows EDGE` accepts a `RELATE` between any two records at
//! all. A graph names its endpoints, and that difference in what a caller may
//! **do** is what earns `GRAPH` its own word, on the same test `BUCKET` and
//! `COLLECTION` passed. The refusal itself is a later slice; what is proven here
//! is that the declaration is taken, stored, reported, and survives the round
//! trip in the direction it was written.
//!
//! # The failure this file exists for
//!
//! `is_edge()` was `kind == TableKind::Edge`, so a graph answered **false** at
//! every gate that asked it — `RELATE`, traversal, the report, the writer. A
//! declared graph would have been a table nothing could write to and nothing
//! could walk, and none of that is a compile error: the variant is new, so every
//! `matches!` was already exhaustive and every equality against `Edge` quietly
//! kept its old answer. The predicate was split into `holds_edges()` (*are the
//! records edges*) and `is_edge()` (*which word declared it*), and the test that
//! catches the regression is the plain one near the bottom of this file: relate
//! two records through a graph and walk back to them.
//!
//! The endpoint refusal (C2) and the ordered endpoint index (C3) are later
//! waves. Nothing here asserts them, because a test that passes for a reason
//! nobody built yet is worse than a missing one.

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

/// A tenancy with two ordinary tables for a graph to join.
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

/// The `graph` part of an `INFO FOR TABLE` report.
fn declared(session: &mut Session<'_>, table: &str) -> BTreeMap<String, Value> {
    let described = report(session, &format!("INFO FOR TABLE {table};"));
    let Value::Object(fields) = &described else {
        panic!("expected an object, got {described:?}");
    };
    match fields.get("graph") {
        Some(Value::Object(graph)) => graph.clone(),
        other => panic!("no graph declaration in the report: {other:?}"),
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
fn a_graph_reports_the_pair_it_joins_in_the_direction_it_was_declared() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE GRAPH wrote FROM users TO posts;")
        .unwrap();
    session
        .run("DEFINE GRAPH about FROM posts TO users;")
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
fn a_graph_reports_the_order_its_edges_are_held_in_and_which_way_it_runs() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE GRAPH recent FROM users TO posts (at datetime) ORDER BY at DESC;")
        .unwrap();
    session
        .run("DEFINE GRAPH earliest FROM users TO posts (at datetime) ORDER BY at;")
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
fn a_graph_declared_without_an_order_reports_none() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE GRAPH follows FROM users TO users;")
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
        .run("DEFINE GRAPH wrote FROM users TO nowhere;")
        .unwrap_err();
    let Error::Unknown { entity, name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "table");
    assert_eq!(name, "nowhere");

    // The endpoints resolve *before* the table is created, and this is the half
    // that matters: a graph left standing with a dangling endpoint could never
    // refuse a `RELATE` against it, which is the entire capability the word was
    // added for. A refusal that still created the table would be worse than no
    // refusal, because the store would then hold a graph that cannot keep its
    // own promise.
    assert!(!tables(&mut session).contains(&"wrote".to_owned()));
}

#[test]
fn an_order_naming_a_field_the_graph_does_not_declare_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let error = session
        .run("DEFINE GRAPH recent FROM users TO posts (at datetime) ORDER BY seen DESC;")
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
fn a_graph_accepts_relate_and_is_walked_the_way_an_edge_table_is() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH follows FROM users TO users;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             CREATE users:3 = { handle: 'katherine' };\n\
             RELATE users:1->follows->users:2;\n\
             RELATE users:2->follows->users:3;",
        )
        .unwrap();

    // This is the regression test named in the module header. Both gates below
    // asked `is_edge()`, which a graph answered false, so the first `RELATE`
    // would have been refused and the walk would have returned nothing — with
    // the catalog, the parser and every match arm compiling perfectly.
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
fn dropping_a_graph_undefines_the_table_it_is() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE GRAPH follows FROM users TO users;")
        .unwrap();
    assert!(tables(&mut session).contains(&"follows".to_owned()));

    // `DROP GRAPH` is `DROP TABLE` under another word, because the words
    // undefine the same catalog entry — the same arm `TABLE`, `SPACE` and
    // `BUCKET` already share.
    session.run("DROP GRAPH follows;").unwrap();
    assert!(!tables(&mut session).contains(&"follows".to_owned()));
}
