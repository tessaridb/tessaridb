//! `DEFINE GRAPH` — a graph that holds its own records.
//!
//! # What the word buys
//!
//! An **object with a node space**. The word arrived in three rounds and only
//! the third is the feature.
//!
//! First it named a pair of endpoints — a constraint wearing a structure's name,
//! rejected because nothing could enumerate, drop or question the thing it
//! claimed to create. Second it became an object other tables could join with
//! `IN social`, which fixed enumeration and dropping and left the graph unable
//! to hold a single record of its own: a caller still had to declare a table
//! before writing a node, so the word named a structure and delivered a
//! membership flag. That was rejected in the same words as the first.
//!
//! Third, and what is asserted here, the graph owns a collection — so
//! `DEFINE GRAPH social; CREATE social:1 = { … };` is the whole script, and
//! attaching an existing table with `IN social` becomes the option it was always
//! described as. The declared engines are symmetrical again: `DEFINE VECTOR
//! embeddings` is written into as `embeddings`, `DEFINE GEO places` as `places`,
//! and now `DEFINE GRAPH social` as `social`.
//!
//! # Why the earlier criterion did not catch it
//!
//! It read *"a test declares a graph, writes nodes and edges into it, walks it,
//! drops it"*, and writing into a table the caller had declared and marked `IN
//! social` satisfies every word. The criterion never asked **who owns the
//! record**, so the node space could be absent while the test passed. The
//! replacement asserts an absence instead — no table statement anywhere in the
//! script — because that is the only half a member table cannot satisfy.
//!
//! # What is not asserted
//!
//! An edge as a **record**: `knows` is adjacency, nothing selects from it, and
//! its properties live in the adjacency value. Whether a graph should own its
//! edges the way it now owns its nodes is a second node-space question with its
//! own storage consequences, and it is open rather than answered here.
//!
//! Questions about the whole — path, degree, components — remain out of scope
//! for the reason they always were: a different execution model, bounded by the
//! reachable subgraph rather than by a stated depth.

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

    // Existing, not absent — the distinction is the whole point of the word: a
    // graph you have just declared is a thing you hold, so the first thing
    // anyone does after declaring one must not read as a failure.
    //
    // And not *empty*, which is the change this wave made. A fresh graph already
    // holds one member: the collection its own nodes live in, carrying the
    // graph's own name. Before that collection existed the listing here was `[]`
    // and the graph could hold nothing at all until the caller declared a table
    // of their own.
    assert_eq!(members(&mut session, "social"), vec!["social".to_owned()]);
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
    assert_eq!(
        listed,
        // `social` is the graph's own node collection, listed beside the two
        // tables the caller attached. All three are members by the same stored
        // fact, which is why the listing does not separate them.
        vec![
            "company".to_owned(),
            "person".to_owned(),
            "social".to_owned()
        ]
    );

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

    // Two graphs, one name, no shadowing — and each lists only its own members:
    // the table attached in that database, and that database's own node
    // collection. A membership resolved against the wrong tenancy would show up
    // exactly here, as one graph claiming the other's table.
    //
    // The two `social` collections are the sharper half now. They carry the same
    // name in two databases, as any two tables may, and neither graph lists the
    // other's — so the node space is scoped where the graph is rather than being
    // one collection both graphs reach.
    let mut listed = members(&mut session, "social");
    listed.sort();
    assert_eq!(listed, vec!["company".to_owned(), "social".to_owned()]);

    session.run("USE DATABASE social;").unwrap();
    let mut listed = members(&mut session, "social");
    listed.sort();
    assert_eq!(listed, vec!["person".to_owned(), "social".to_owned()]);
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

/// How many records the script's last statement answered with.
fn rows(session: &mut Session<'_>, script: &str) -> usize {
    let outcomes = session.run(script).unwrap();
    match outcomes.last() {
        Some(Outcome::Records { records, .. }) => records.len(),
        other => panic!("expected records, got {other:?}"),
    }
}

#[test]
fn a_graph_holds_its_own_records_with_no_table_declared_by_the_caller() {
    let store = store();
    let mut session = ready(&store);

    // THE CRITERION, and the absence below is the load-bearing half of it: there
    // is no `DEFINE TABLE`, no `DEFINE COLLECTION`, and no `IN social` anywhere
    // in this script. Every statement names only the graph.
    //
    // The criterion this replaces read "a test declares a graph, writes nodes
    // and edges into it, walks it, drops it" — and writing into a table the
    // caller had declared and marked `IN social` satisfied every word of it.
    // That is why the node space could be missing while the criterion passed,
    // twice. A test that cannot fail for the reason the feature exists is not a
    // test of that feature.
    session
        .run(
            "DEFINE GRAPH social;\n\
             CREATE social:1 = { name: 'ada', team: 'core' };\n\
             CREATE social:2 = { name: 'grace', team: 'core' };\n\
             CREATE social:3 = { name: 'katherine', team: 'flight' };",
        )
        .unwrap();

    // Insert, then read back by identity and by condition — the three operations
    // the objection actually named.
    assert_eq!(rows(&mut session, "SELECT * FROM social:1;"), 1);
    assert_eq!(
        rows(&mut session, "SELECT * FROM social WHERE team = 'core';"),
        2
    );
    assert_eq!(rows(&mut session, "SELECT * FROM social;"), 3);

    // An identity the store produces, because a graph whose nodes must all be
    // named by the caller is only half a store.
    session.run("CREATE social = { name: 'edith' };").unwrap();
    assert_eq!(rows(&mut session, "SELECT * FROM social;"), 4);

    // Delete.
    session.run("DELETE social:3;").unwrap();
    assert_eq!(rows(&mut session, "SELECT * FROM social;"), 3);
}

#[test]
fn a_graph_relates_and_walks_its_own_nodes() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             CREATE social:1 = { name: 'ada' };\n\
             CREATE social:2 = { name: 'grace' };\n\
             CREATE social:3 = { name: 'katherine' };\n\
             DEFINE EDGE knows IN social FROM social TO social;\n\
             RELATE social:1->knows->social:2;\n\
             RELATE social:2->knows->social:3;",
        )
        .unwrap();

    // The edge kind joins the graph's own collection to itself, which is the
    // ordinary shape for a graph whose nodes are one kind of thing — and it is
    // only expressible because that collection exists.
    assert_eq!(
        rows(&mut session, "SELECT * FROM social:1->knows->social;"),
        1
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM social:3<-knows<-social;"),
        1
    );

    // And the bounded walk, over nodes nothing but the graph declared.
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM social:1->knows->social DEPTH 2;"
        ),
        2
    );
}

#[test]
fn a_graph_holding_only_its_own_nodes_drops_cleanly() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             CREATE social:1 = { name: 'ada' };",
        )
        .unwrap();

    // The trap this asserts against: the graph's own collection belongs to the
    // graph, so a drop that counted it as a dependant would refuse — naming a
    // table the caller never declared and cannot drop by name, leaving every
    // graph this store creates permanently undroppable. That is the shape the
    // bucket's companion chunk table already found once.
    session.run("DROP GRAPH social;").unwrap();

    // Both names are released, and that is two facts rather than one: the graph
    // reservation and the collection's. A drop that freed only the graph would
    // fail here on the second statement instead of the first.
    session.run("DEFINE GRAPH social;").unwrap();
    session.run("CREATE social:1 = { name: 'ada' };").unwrap();
}

#[test]
fn a_graph_is_still_refused_a_drop_while_a_table_the_caller_attached_belongs() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             DEFINE TABLE person (name string) IN social;",
        )
        .unwrap();

    // The other half of the partition above. Excluding the graph's own
    // collection from the dependant count must not excuse the tables somebody
    // else attached, and the refusal still names one of them rather than the
    // collection — which is what tells the two apart in the message a caller
    // actually reads.
    let error = session.run("DROP GRAPH social;").unwrap_err();
    let Error::StillDepended {
        name, first, count, ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(name, "social");
    assert_eq!(first, "person");
    assert_eq!(*count, 1);
}

#[test]
fn a_table_already_holding_the_graphs_name_refuses_the_declaration_whole() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE COLLECTION social;").unwrap();

    // The graph's node collection takes the graph's name, so a table already
    // standing there is a collision rather than something to adopt: adopting it
    // would hand the graph records written before it existed and make `DROP
    // GRAPH` delete a table the caller declared themselves.
    session.run("DEFINE GRAPH social;").unwrap_err();

    // And the graph row goes with it. A graph left behind by a half-applied
    // declaration would be a structure with no node space — exactly the state
    // this wave exists to remove — and nothing would report it as wrong.
    let error = session.run("INFO FOR GRAPH social;").unwrap_err();
    let Error::Unknown { entity, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "graph");
}

#[test]
fn a_graphs_own_collection_is_not_a_table_the_caller_can_drop() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE GRAPH social;\nCREATE social:1 = { name: 'ada' };")
        .unwrap();

    // One statement was enough to undo the node space: `DROP TABLE social`
    // removed the collection and left the graph declared, still answering
    // `INFO FOR GRAPH`, and able to hold no record. Nothing was in an error
    // state, which is why the refusal has to exist rather than be documented.
    let error = session.run("DROP TABLE social;").unwrap_err();
    let Error::TableBelongsToGraph { table, graph, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(table, "social");
    assert_eq!(graph, "social");

    // The message names the statement to write instead, which is what makes the
    // refusal a signpost rather than a wall.
    assert!(error.to_string().contains("DROP GRAPH social"), "{error}");

    // And the collection is still there afterwards — a refusal that had already
    // dropped the table would be the same defect with a message on top.
    assert_eq!(rows(&mut session, "SELECT * FROM social;"), 1);
}

#[test]
fn a_table_the_caller_attached_still_drops_on_its_own() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             DEFINE TABLE person (name string) IN social;",
        )
        .unwrap();

    // `IN` is a clause the caller wrote and may withdraw. Refusing this would
    // make the graph's boundary a trap rather than a structure, and the
    // partition in `drop_graph` already draws the same line the other way.
    session.run("DROP TABLE person;").unwrap();

    // The graph outlives it, and now holds only its own collection.
    session.run("INFO FOR GRAPH social;").unwrap();
    session.run("DROP GRAPH social;").unwrap();
}

#[test]
fn the_refusal_reads_the_graphs_name_rather_than_assuming_the_tables() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE GRAPH social;\n\
             DEFINE TABLE person (name string) IN social;",
        )
        .unwrap();

    // The check is `table.graph names a graph whose name is this table's name`,
    // and both halves are load-bearing. A table attached with `IN` carries the
    // same `graph` id and differs only by name, so a check that stopped at the
    // id would refuse this drop too — which the test above proves it does not.
    // Asserted here as its own claim so a future change to name reservation,
    // which is what makes the name test sound, fails loudly rather than
    // quietly widening the refusal.
    let error = session.run("DROP TABLE social;").unwrap_err();
    let Error::TableBelongsToGraph { table, graph, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(table, graph);
}
