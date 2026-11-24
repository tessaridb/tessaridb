//! `FROM GRAPH … BY` — whole-graph analytics over a declared graph (ADR-0144).
//!
//! # Why oracle graphs and not generated ones
//!
//! Every assertion here is against a graph small enough that the expected
//! answer is written out by hand from the definition of the algorithm — a
//! two-component graph for `wcc`, a line for `sssp`, a bridge graph for
//! `betweenness`. An algorithm that is merely plausible answers the same as
//! a correct one on random data; it cannot answer the same as a correct one
//! on a graph whose answer was written down before the code ran.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]
// The oracle scores under test are small counts; the cast lints are about
// precision at magnitudes this file never reaches.
#![allow(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// 1→2→3→4 with a cross edge 2→4, plus an isolated 5.
fn line(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE GRAPH web;\n\
             DEFINE TABLE person (name string) IN web;\n\
             DEFINE EDGE knows IN web FROM person TO person;\n\
             CREATE person:1 = { name: 'Ada' };\n\
             CREATE person:2 = { name: 'Grace' };\n\
             CREATE person:3 = { name: 'Katherine' };\n\
             CREATE person:4 = { name: 'Dorothy' };\n\
             CREATE person:5 = { name: 'Mary' };\n\
             RELATE person:1->knows->person:2;\n\
             RELATE person:2->knows->person:3;\n\
             RELATE person:3->knows->person:4;\n\
             RELATE person:2->knows->person:4;",
        )
        .unwrap();
    session
}

/// The (node id, score) rows an algorithm answered, in the order written.
fn scored(session: &mut Session<'_>, read: &str) -> Vec<(i64, f64)> {
    let outcomes = session.run_now(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("{read}: {:?}", outcomes.last());
    };
    records
        .iter()
        .map(|(_, value)| match value {
            Value::Object(fields) => {
                let node = match fields.get("node") {
                    Some(Value::Record(reference)) => match &reference.id {
                        RecordId::Int(n) => *n,
                        other => panic!("not an int id: {other:?}"),
                    },
                    other => panic!("no node: {other:?}"),
                };
                let score = match fields.get("score").or_else(|| fields.get("cost")) {
                    Some(Value::Number(Number::Integer(n))) => {
                        f64::from(i32::try_from(*n).unwrap())
                    }
                    Some(Value::Number(Number::Float(f))) => *f,
                    other => panic!("no score: {other:?}"),
                };
                (node, score)
            }
            other => panic!("not a row: {other:?}"),
        })
        .collect()
}

#[test]
fn sssp_answers_the_fewest_steps_cost_from_the_start() {
    let store = store();
    let mut session = line(&store);
    let found = scored(
        &mut session,
        "SELECT * FROM GRAPH web BY sssp FROM person:1;",
    );
    // 1→2 (1), 2→3 (2), 3→4 (3), 2→4 (2). Node 5 is unreachable and not
    // in the answer.
    assert_eq!(found, vec![(1, 0.0), (2, 1.0), (3, 2.0), (4, 2.0)]);
}

#[test]
fn wcc_labels_the_components_by_their_smallest_member() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE GRAPH web;\n\
             DEFINE TABLE person (name string) IN web;\n\
             DEFINE EDGE knows IN web FROM person TO person;\n\
             CREATE person:1 = { name: 'Ada' };\n\
             CREATE person:2 = { name: 'Grace' };\n\
             CREATE person:3 = { name: 'Katherine' };\n\
             CREATE person:4 = { name: 'Dorothy' };\n\
             RELATE person:1->knows->person:2;\n\
             RELATE person:3->knows->person:4;",
        )
        .unwrap();
    let found = scored(&mut session, "SELECT * FROM GRAPH web BY wcc;");
    // Two components; the label of each is its smallest member's position in
    // the graph's node order (sorted by id: 1→position 0, 3→position 2).
    assert_eq!(found, vec![(1, 0.0), (2, 0.0), (3, 2.0), (4, 2.0)]);
}

#[test]
fn pagerank_ranks_a_hub_above_its_leaves_and_sums_to_one() {
    let store = store();
    let mut session = line(&store);
    let found = scored(&mut session, "SELECT * FROM GRAPH web BY pagerank;");
    assert_eq!(found.len(), 5);
    let rank = |id: i64| {
        found
            .iter()
            .find(|(node, _)| *node == id)
            .map(|(_, s)| *s)
            .unwrap()
    };
    // Node 4 collects from both 3 and the hub 2, so it ranks above the hub;
    // the hub ranks above its other leaf 3 and the source 1. The dangling
    // node 5 keeps the mass honest.
    assert!(rank(4) > rank(2));
    assert!(rank(2) > rank(3));
    assert!(rank(2) > rank(1));
    let sum: f64 = found.iter().map(|(_, s)| s).sum();
    assert!((sum - 1.0).abs() < 1e-9, "ranks sum to {sum}");
}

#[test]
fn betweenness_marks_the_bridge_highest() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE GRAPH web;\n\
             DEFINE TABLE person (name string) IN web;\n\
             DEFINE EDGE knows IN web FROM person TO person;\n\
             CREATE person:1 = { name: 'Ada' };\n\
             CREATE person:2 = { name: 'Grace' };\n\
             CREATE person:3 = { name: 'Katherine' };\n\
             CREATE person:4 = { name: 'Dorothy' };\n\
             CREATE person:5 = { name: 'Mary' };\n\
             RELATE person:1->knows->person:2;\n\
             RELATE person:2->knows->person:3;\n\
             RELATE person:3->knows->person:4;\n\
             RELATE person:4->knows->person:5;",
        )
        .unwrap();
    let found = scored(&mut session, "SELECT * FROM GRAPH web BY betweenness;");
    let rank = |id: i64| {
        found
            .iter()
            .find(|(node, _)| *node == id)
            .map(|(_, s)| *s)
            .unwrap()
    };
    // 3 sits on every 1→4, 1→5, 2→4 and 2→5 shortest path: strictly highest.
    assert!(rank(3) > rank(1));
    assert!(rank(3) > rank(2));
    assert!(rank(3) > rank(4));
    assert!(rank(3) > rank(5));
}

#[test]
fn harmonic_sums_inverse_distances_and_cdlp_converges_one_label() {
    let store = store();
    let mut session = line(&store);
    let harmonic = scored(&mut session, "SELECT * FROM GRAPH web BY harmonic;");
    let reach = |id: i64| {
        harmonic
            .iter()
            .find(|(node, _)| *node == id)
            .map(|(_, s)| *s)
            .unwrap()
    };
    // Node 1 reaches 2 (1), 3 (1/2) and 4 (1/2): 2. Node 5 reaches nothing.
    assert!((reach(1) - 2.0).abs() < 1e-9);
    assert!((reach(5) - 0.0).abs() < 1e-9);

    // The dense component converges to one label; the isolated node 5 keeps
    // its own — two labels, not one.
    let cdlp = scored(&mut session, "SELECT * FROM GRAPH web BY cdlp;");
    let as_int = |value: f64| value.round() as i64;
    let labels: std::collections::BTreeSet<i64> = cdlp.iter().map(|(_, s)| as_int(*s)).collect();
    assert_eq!(labels.len(), 2);
    let dense: std::collections::BTreeSet<i64> = cdlp
        .iter()
        .filter(|(node, _)| *node != 5)
        .map(|(_, s)| as_int(*s))
        .collect();
    assert_eq!(dense.len(), 1);
}

#[test]
fn the_graph_read_refuses_its_wrong_shapes_by_name() {
    let store = store();
    let mut session = line(&store);

    for (read, check) in [
        (
            "SELECT * FROM GRAPH missing BY pagerank;",
            "GraphNotDeclared",
        ),
        ("SELECT * FROM GRAPH web BY nosuch;", "UnknownAlgorithm"),
        ("SELECT * FROM GRAPH web BY sssp;", "AlgorithmNeedsStart"),
        (
            "SELECT * FROM GRAPH web BY pagerank FROM person:1;",
            "AlgorithmTakesNoStart",
        ),
        (
            "SELECT * FROM GRAPH web BY pagerank USING nosuch;",
            "EdgeKindNotInGraph",
        ),
    ] {
        let error = session.run_now(read).unwrap_err();
        let named = match check {
            "GraphNotDeclared" => matches!(error, Error::GraphNotDeclared { .. }),
            "UnknownAlgorithm" => matches!(error, Error::UnknownAlgorithm { .. }),
            "AlgorithmNeedsStart" => matches!(error, Error::AlgorithmNeedsStart { .. }),
            "AlgorithmTakesNoStart" => matches!(error, Error::AlgorithmTakesNoStart { .. }),
            "EdgeKindNotInGraph" => matches!(error, Error::EdgeKindNotInGraph { .. }),
            _ => false,
        };
        assert!(named, "{read}: {error}");
    }
}

#[test]
fn using_restricts_the_pass_to_one_edge_kind() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE GRAPH web;\n\
             DEFINE TABLE person (name string) IN web;\n\
             DEFINE EDGE knows IN web FROM person TO person;\n\
             DEFINE EDGE works_with IN web FROM person TO person;\n\
             CREATE person:1 = { name: 'Ada' };\n\
             CREATE person:2 = { name: 'Grace' };\n\
             CREATE person:3 = { name: 'Katherine' };\n\
             RELATE person:1->knows->person:2;\n\
             RELATE person:2->knows->person:3;\n\
             RELATE person:2->works_with->person:3;",
        )
        .unwrap();
    // Over knows alone, 3 is two steps from 1 even though works_with would
    // also reach it — the USING shape walks exactly the kind it names.
    let found = scored(
        &mut session,
        "SELECT * FROM GRAPH web BY sssp FROM person:1 USING knows;",
    );
    assert_eq!(found, vec![(1, 0.0), (2, 1.0), (3, 2.0)]);
}
