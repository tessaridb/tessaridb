//! `DEFINE GRAPH` — the structure node tables belong to.
//!
//! # What the word buys
//!
//! An **object**. Before it, "the social graph" was a fact in somebody's head
//! about which tables were related: nothing could enumerate it, nothing could
//! drop it, and nothing could be asked a question about it. That is why the word
//! failed the doorway test in its first shape, where it merely named a pair of
//! endpoints — a constraint wearing a structure's name. What is asserted here is
//! the object: it is created, it is listed, its members are reported, and it
//! refuses to disappear out from under them.
//!
//! # What is not asserted
//!
//! Adjacency, `DEFINE EDGE … IN`, bounded walks, and questions about the whole
//! (path, degree, components). Those need the adjacency keyspace, and a test
//! that passed for a reason nobody has built yet would be worse than a missing
//! one — which is the same reason the endpoint refusal was left unasserted until
//! the wave that actually built it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
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
             DEFINE DATABASE social; USE DATABASE social;",
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

/// The tables `INFO FOR GRAPH` says belong to a graph.
fn members(session: &mut Session<'_>, graph: &str) -> Vec<String> {
    let described = report(session, &format!("INFO FOR GRAPH {graph};"));
    let Value::Object(fields) = &described else {
        panic!("expected an object, got {described:?}");
    };
    match fields.get("tables") {
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| match name {
                Value::String(text) => text.clone(),
                other => panic!("a table name that is not text: {other:?}"),
            })
            .collect(),
        other => panic!("no table listing: {other:?}"),
    }
}

/// The graph a table reports belonging to, as `INFO FOR TABLE` gives it.
fn membership(session: &mut Session<'_>, table: &str) -> Option<Value> {
    let described = report(session, &format!("INFO FOR TABLE {table};"));
    let Value::Object(fields) = &described else {
        panic!("expected an object, got {described:?}");
    };
    fields.get("graph").cloned()
}

#[test]
fn a_graph_declared_and_never_populated_is_an_object_that_exists() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE GRAPH social;").unwrap();

    // Empty and existing, not absent. The distinction is the whole point of the
    // word: a graph you have just declared is a thing you hold, so the first
    // thing anyone does after declaring one must not read as a failure.
    assert!(members(&mut session, "social").is_empty());
}

#[test]
fn a_table_says_which_graph_it_belongs_to_and_the_graph_lists_it_back() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             DEFINE TABLE person (name string) IN social;\n\
             DEFINE TABLE company (name string) IN social;\n\
             DEFINE TABLE audit (at datetime);",
        )
        .unwrap();

    // Both directions, because they are two facts and only one is stored: the
    // membership lives on the table, and the graph's listing is derived by
    // filtering. A listing held separately on the graph could disagree with the
    // tables it names, which is why it is not held that way.
    let mut listed = members(&mut session, "social");
    listed.sort();
    assert_eq!(listed, vec!["company".to_owned(), "person".to_owned()]);

    // The membership is reported as the id the catalog holds, so this asserts
    // the relation rather than the number: the two members agree, and the table
    // outside the graph carries nothing. A filter written against the wrong
    // field would pass the first assertion and fail the second.
    assert_eq!(
        membership(&mut session, "person"),
        membership(&mut session, "company")
    );
    assert!(membership(&mut session, "person").is_some());
    assert_eq!(membership(&mut session, "audit"), None);
}

#[test]
fn a_membership_naming_no_graph_is_refused_and_leaves_no_table_behind() {
    let store = store();
    let mut session = ready(&store);

    let error = session
        .run("DEFINE TABLE person (name string) IN nowhere;")
        .unwrap_err();
    let Error::Unknown { entity, name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "graph");
    assert_eq!(name, "nowhere");

    // The membership resolves *before* the table is created, and that ordering
    // is the half that matters: a table left standing with a membership nothing
    // resolves belongs to no graph anyone can name, so `INFO FOR GRAPH` would
    // never list it and nothing would report it as lost.
    let listing = report(&mut session, "INFO FOR DATABASE;");
    let Value::Object(fields) = &listing else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("tables"), Some(&Value::Array(Vec::new())));
}

#[test]
fn a_graph_refuses_to_be_dropped_while_a_table_still_belongs_to_it() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             DEFINE TABLE person (name string) IN social;",
        )
        .unwrap();

    // Refusing rather than orphaning. A dropped graph whose members kept their
    // membership would leave every one of them pointing at an id nothing
    // resolves, and the symptom would surface later as a walk that finds no
    // graph rather than now, as the drop that caused it.
    let error = session.run("DROP GRAPH social;").unwrap_err();
    let Error::StillDepended { name, first, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(name, "social");
    assert_eq!(first, "person");

    session.run("DROP TABLE person;").unwrap();
    session.run("DROP GRAPH social;").unwrap();

    // And the name is released, so the graph can be declared again. A name still
    // claimed by a dropped graph would make the second declaration fail as taken
    // by something the store no longer has.
    session.run("DEFINE GRAPH social;").unwrap();
}

#[test]
fn two_databases_may_each_hold_a_graph_of_the_same_name() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             DEFINE TABLE person (name string) IN social;\n\
             DEFINE DATABASE other; USE DATABASE other;\n\
             DEFINE GRAPH social;\n\
             DEFINE TABLE company (name string) IN social;",
        )
        .unwrap();

    // Two graphs, one name, no shadowing — and each lists only its own. A
    // membership resolved against the wrong tenancy would show up exactly here,
    // as one graph claiming the other's table.
    assert_eq!(members(&mut session, "social"), vec!["company".to_owned()]);
    session.run("USE DATABASE social;").unwrap();
    assert_eq!(members(&mut session, "social"), vec!["person".to_owned()]);
}

#[test]
fn a_second_graph_of_the_same_name_in_one_database_is_refused() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE GRAPH social;").unwrap();

    let error = session.run("DEFINE GRAPH social;").unwrap_err();
    let Error::Store(tessari_storage::Error::NameTaken { .. }) = &error else {
        panic!("{error}");
    };

    // Unless the statement said it expected the name to be there already, which
    // is the same contract every other `DEFINE` keeps.
    session.run("DEFINE GRAPH IF NOT EXISTS social;").unwrap();
}

#[test]
fn asking_about_a_graph_that_was_never_declared_refuses() {
    let store = store();
    let mut session = ready(&store);

    let error = session.run("INFO FOR GRAPH nowhere;").unwrap_err();
    let Error::Unknown { entity, name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "graph");
    assert_eq!(name, "nowhere");

    // The asymmetry with the empty-graph test above is deliberate: *declared and
    // empty* is a graph, *never declared* is not, and a report that answered
    // both with an empty list would make a misspelled name look like an empty
    // structure.
    let error = session.run("DROP GRAPH nowhere;").unwrap_err();
    assert!(matches!(error, Error::Unknown { .. }), "{error}");
}
