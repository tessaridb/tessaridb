//! `DEFINE EDGE` — the word, and the adjacency it names.
//!
//! # What is asserted here that `graph_container.rs` could not assert
//!
//! That one is about the graph as an object: declared, listed, dropped. This one
//! is about the engine underneath it. An edge kind writes adjacency beside the
//! node in both directions, so a walk is a range read over the node's own prefix
//! rather than an index probe and a random read of every edge record.
//!
//! Every assertion here is made **through the language**, not against the bytes.
//! That is deliberate: the bidirectional invariant is what a reader depends on —
//! *if A reaches B, then B is reached from A* — and asserting it as a behaviour
//! survives a change of key layout, while asserting it as a key does not.
//!
//! # The one thing a test like this cannot show
//!
//! That the hop is *cheap*. Six records make an index probe and a range read look
//! identical, and that is precisely why graph engines are argued about with
//! designs rather than with small tests. The cost claim belongs to a counting
//! backend and is not made here.

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

/// A graph with two node kinds and one edge kind between them.
fn social(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE GRAPH org;\n\
             DEFINE TABLE person (name string) IN org;\n\
             DEFINE TABLE company (name string) IN org;\n\
             DEFINE EDGE works_at IN org FROM person TO company;\n\
             CREATE person:1 = { name: 'Ada' };\n\
             CREATE person:2 = { name: 'Grace' };\n\
             CREATE company:1 = { name: 'Analytical' };\n\
             CREATE company:2 = { name: 'Naval' };",
        )
        .unwrap();
    session
}

/// The rows a query answered with.
fn rows(session: &mut Session<'_>, script: &str) -> Vec<Value> {
    let outcomes = session.run(script).unwrap();
    match outcomes.last() {
        Some(Outcome::Records { records, .. }) => {
            records.iter().map(|(_, value)| value.clone()).collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The `name` field of every row, sorted.
fn names(session: &mut Session<'_>, script: &str) -> Vec<String> {
    let mut found: Vec<_> = rows(session, script)
        .iter()
        .map(|row| match row {
            Value::Object(fields) => match fields.get("name") {
                Some(Value::String(text)) => text.clone(),
                other => panic!("no name: {other:?}"),
            },
            other => panic!("not a record: {other:?}"),
        })
        .collect();
    found.sort();
    found
}

#[test]
fn an_edge_is_reachable_from_both_of_the_records_it_joins() {
    let store = store();
    let mut session = social(&store);
    session
        .run(
            "RELATE person:1->works_at->company:1;\n\
             RELATE person:2->works_at->company:1;",
        )
        .unwrap();

    // The bidirectional invariant, stated as the reader depends on it: if Ada
    // reaches Analytical, Analytical is reached from Ada. A store that wrote only
    // the forward entry would pass the first of these and answer the second by
    // reading everything — and "who works here" is the question an org chart is
    // actually asked.
    assert_eq!(
        names(&mut session, "SELECT * FROM person:1->works_at->company;"),
        vec!["Analytical".to_owned()]
    );
    assert_eq!(
        names(&mut session, "SELECT * FROM company:1<-works_at<-person;"),
        vec!["Ada".to_owned(), "Grace".to_owned()]
    );

    // And a node nothing reaches has an empty neighbourhood rather than an error:
    // an empty range read and a missing one must not look alike.
    assert!(names(&mut session, "SELECT * FROM company:2<-works_at<-person;").is_empty());
}

#[test]
fn an_edge_carries_its_own_properties_in_both_directions() {
    let store = store();
    let mut session = social(&store);
    session
        .run("RELATE person:1->works_at->company:1 = { since: 1843 };")
        .unwrap();

    // The properties live in the adjacency value on both entries, so a reverse
    // read is as complete as a forward one. A design that stored them once, or
    // pointed at an edge record, would either lose them here or pay a random read
    // per neighbour to fetch them.
    for direction in [
        "SELECT * FROM person:1->works_at;",
        "SELECT * FROM company:1<-works_at;",
    ] {
        let found = rows(&mut session, direction);
        assert_eq!(found.len(), 1, "{direction}");
        let Value::Object(fields) = &found[0] else {
            panic!("not an edge: {found:?}");
        };
        assert_eq!(
            fields.get("since"),
            Some(&Value::from(1843_i64)),
            "{direction}"
        );
        // The endpoints are read out of the key rather than fetched, and they
        // keep their sense whichever way the arrow was written.
        assert!(fields.contains_key("out"), "{direction}");
        assert!(fields.contains_key("in"), "{direction}");
    }
}

#[test]
fn relating_the_same_pair_twice_replaces_rather_than_doubles() {
    let store = store();
    let mut session = social(&store);
    session
        .run(
            "RELATE person:1->works_at->company:1 = { since: 1843 };\n\
             RELATE person:1->works_at->company:1 = { since: 1852 };",
        )
        .unwrap();

    // An edge is identified by its endpoints, so adjacency is a set. A second
    // entry under the same pair would make a hop return one neighbour twice, and
    // a count over a walk would be quietly wrong.
    let found = rows(&mut session, "SELECT * FROM person:1->works_at;");
    assert_eq!(found.len(), 1);
    let Value::Object(fields) = &found[0] else {
        panic!("not an edge");
    };
    assert_eq!(fields.get("since"), Some(&Value::from(1852_i64)));
}

#[test]
fn a_relation_off_the_declared_pair_is_refused_in_both_of_the_ways_it_can_be_wrong() {
    let store = store();
    let mut session = social(&store);

    // The declared pair is accepted first, so the refusals below cannot pass by
    // refusing everything.
    session
        .run("RELATE person:1->works_at->company:1;")
        .unwrap();

    let error = session
        .run("RELATE person:1->works_at->person:2;")
        .unwrap_err();
    assert!(
        matches!(error, Error::EndpointsNotDeclared { .. }),
        "{error}"
    );

    // Right pair, wrong way round — an unordered check would accept this, and the
    // adjacency written would say a company works at a person.
    let error = session
        .run("RELATE company:1->works_at->person:1;")
        .unwrap_err();
    assert!(
        matches!(error, Error::EndpointsNotDeclared { .. }),
        "{error}"
    );
}

#[test]
fn an_endpoint_outside_the_graph_is_refused_because_that_is_what_bounds_a_walk() {
    let store = store();
    let mut session = social(&store);
    session.run("DEFINE TABLE audit (at datetime);").unwrap();

    // `audit` belongs to no graph. A kind reaching it would let a traversal leave
    // the structure it was told to stay inside and still answer — with records the
    // graph does not contain.
    let error = session
        .run("DEFINE EDGE logged IN org FROM person TO audit;")
        .unwrap_err();
    let Error::EndpointOutsideGraph { table, graph, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(table, "audit");
    assert_eq!(graph, "org");

    // And the refused declaration left no kind behind. The refusal is `Unknown`
    // rather than "not an edge table", which is the stronger statement: nothing
    // of that name exists at all, so the declaration wrote nothing before it
    // failed.
    let error = session
        .run("RELATE person:1->logged->company:1;")
        .unwrap_err();
    let Error::Unknown { name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(name, "logged");
}

#[test]
fn a_deleted_node_drops_out_of_the_walk_rather_than_failing_it() {
    let store = store();
    let mut session = social(&store);
    session
        .run(
            "RELATE person:1->works_at->company:1;\n\
             RELATE person:1->works_at->company:2;\n\
             DELETE company:2;",
        )
        .unwrap();

    // A record can go while an entry still names it. That is a state of the
    // graph, not a failure of the query — the alternative is a read that breaks
    // because of a write it has nothing to do with.
    assert_eq!(
        names(&mut session, "SELECT * FROM person:1->works_at->company;"),
        vec!["Analytical".to_owned()]
    );
    // The entry itself survives its dangling far side, exactly as an edge record
    // does on the edge-table path.
    assert_eq!(
        rows(&mut session, "SELECT * FROM person:1->works_at;").len(),
        2
    );
}

#[test]
fn dropping_an_edge_kind_takes_its_adjacency_with_it() {
    let store = store();
    let mut session = social(&store);
    session
        .run("RELATE person:1->works_at->company:1;")
        .unwrap();
    assert_eq!(
        rows(&mut session, "SELECT * FROM person:1->works_at;").len(),
        1
    );

    session.run("DROP EDGE works_at;").unwrap();

    // The entries go with the kind, in the transaction that dropped it. Entries
    // left behind would point at an id nothing resolves, and a walk would reach
    // through a join that no longer exists.
    let error = session
        .run("SELECT * FROM person:1->works_at;")
        .unwrap_err();
    let Error::Unknown { name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(name, "works_at");

    // And the name is released, so the kind can be declared again over the same
    // pair — with nothing surviving from the first one.
    session
        .run("DEFINE EDGE works_at IN org FROM person TO company;")
        .unwrap();
    assert!(rows(&mut session, "SELECT * FROM person:1->works_at;").is_empty());
}

#[test]
fn a_graph_refuses_to_be_dropped_while_an_edge_kind_belongs_to_it() {
    let store = store();
    let mut session = social(&store);

    // Two dependants, and they are refused separately: the tables were already
    // asserted in `graph_container.rs`, so this drops them first to reach the
    // edge kind's own refusal rather than passing on the earlier one.
    session.run("DROP EDGE works_at;").unwrap();
    session
        .run("DEFINE EDGE works_at IN org FROM person TO company;")
        .unwrap();

    let error = session.run("DROP GRAPH org;").unwrap_err();
    let Error::StillDepended { name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(name, "org");
}

#[test]
fn a_graph_lists_its_edge_kinds_beside_its_node_tables_and_not_among_them() {
    let store = store();
    let mut session = social(&store);

    let outcomes = session.run("INFO FOR GRAPH org;").unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("expected a report");
    };

    // Separate keys, because they are separate things: nothing can select from an
    // edge kind, and a report that folded it in with the tables would invite a
    // reader to try.
    assert_eq!(
        fields.get("tables"),
        Some(&Value::Array(vec![
            Value::from("company"),
            Value::from("person"),
        ]))
    );
    assert_eq!(
        fields.get("edges"),
        Some(&Value::Array(vec![Value::from("works_at")]))
    );
}

#[test]
fn two_edge_kinds_over_the_same_pair_do_not_see_each_others_edges() {
    let store = store();
    let mut session = social(&store);
    session
        .run(
            "DEFINE EDGE founded IN org FROM person TO company;\n\
             RELATE person:1->works_at->company:1;\n\
             RELATE person:2->founded->company:1;",
        )
        .unwrap();

    // The edge kind sits in the key between the node and the neighbour, so one
    // kind's entries are a narrower prefix than the node's. A layout that put the
    // kind above the node — or left it out — would answer both of these with both
    // edges, and the count would look plausible.
    assert_eq!(
        names(&mut session, "SELECT * FROM company:1<-works_at<-person;"),
        vec!["Ada".to_owned()]
    );
    assert_eq!(
        names(&mut session, "SELECT * FROM company:1<-founded<-person;"),
        vec!["Grace".to_owned()]
    );
}
