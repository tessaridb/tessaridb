//! A quantized vector index: one byte per component, rescored on full vectors.
//!
//! The codes choose which records a walk tries; the records' own full-precision
//! vectors decide their order. What is asserted here is that division of labour:
//! a fixture whose codes cannot tell the nearest records apart still answers with
//! the right ones, in the right order, and the store reports that it is
//! quantized and what a node costs.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;",
        )
        .unwrap();
    session
}

/// Forty vectors whose third component differs by a tenth: every one codes to
/// nearly the same byte over a range of a thousand, so the codes tie and only
/// the full vectors can say which is nearest.
fn crowded(session: &mut Session<'_>, table: &str) {
    let mut script = String::new();
    for n in 0..40 {
        let third = 500.0 + f64::from(n) * 0.1;
        script.push_str(&format!(
            "CREATE {table}:{n} = {{ vector: [0.0, 1000.0, {third:.1}] }};\n"
        ));
    }
    session.run(&script).unwrap();
}

fn ids_and_path(session: &mut Session<'_>, script: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        plan.access,
    )
}

#[test]
fn a_quantized_read_is_rescored_on_the_full_vectors() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE VECTOR crowd DIMENSION 3 DISTANCE euclidean QUANTIZED;")
        .unwrap();
    crowded(&mut session, "crowd");
    let read =
        "SELECT * FROM crowd ORDER BY vector::euclidean(vector, [0.0, 1000.0, 500.93]) LIMIT 3";
    let (exact, _) = ids_and_path(&mut session, &format!("{read};"));
    assert_eq!(
        exact,
        vec![RecordId::Int(9), RecordId::Int(10), RecordId::Int(8)]
    );
    let (walked, path) = ids_and_path(&mut session, &format!("{read} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Approximate);
    assert_eq!(
        walked, exact,
        "the codes chose the candidates, the full vectors the order"
    );
}

#[test]
fn a_store_reports_that_it_is_quantized_and_what_a_node_costs() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE VECTOR full DIMENSION 3 DISTANCE euclidean;\n\
             DEFINE VECTOR coded DIMENSION 3 DISTANCE euclidean QUANTIZED;",
        )
        .unwrap();
    crowded(&mut session, "full");
    crowded(&mut session, "coded");
    session
        .run("REBUILD INDEX vector ON full; REBUILD INDEX vector ON coded;")
        .unwrap();

    let info = |session: &mut Session<'_>, name: &str| {
        let outcomes = session.run(&format!("INFO FOR VECTOR {name};")).unwrap();
        let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
            panic!("{outcomes:?}");
        };
        fields.clone()
    };
    let full = info(&mut session, "full");
    let coded = info(&mut session, "coded");
    assert_eq!(full.get("quantized"), Some(&Value::Bool(false)));
    assert_eq!(coded.get("quantized"), Some(&Value::Bool(true)));
    let bytes =
        |fields: &std::collections::BTreeMap<String, Value>| match fields.get("vector_bytes") {
            Some(Value::Number(Number::Integer(held))) => *held,
            other => panic!("vector_bytes was {other:?}"),
        };
    // Three components: four bytes of width and twenty-four of floats, against
    // four of width, sixteen of range and three of codes.
    assert_eq!(bytes(&full), 28, "{full:?}");
    assert_eq!(bytes(&coded), 23, "{coded:?}");
    assert!(
        matches!(full.get("node_bytes"), Some(Value::Number(_))),
        "{full:?}"
    );
}

#[test]
fn quantized_is_refused_on_an_index_that_holds_no_vectors() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE COLLECTION people;").unwrap();
    assert!(
        session
            .run("DEFINE INDEX by_city ON people FIELDS city QUANTIZED;")
            .is_err()
    );
}
