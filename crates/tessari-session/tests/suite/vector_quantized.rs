//! A quantized vector index: one byte per component, rescored on full vectors.
//!
//! The codes choose which records a walk tries; the records' own full-precision
//! vectors decide their order. What is asserted here is that division of labour:
//! a fixture whose codes cannot tell the nearest records apart still answers with
//! the right ones, in the right order, and the store reports that it is
//! quantized and what a node costs.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_encoding::StoreKey;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
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
    session.run_now(&script).unwrap();
}

fn ids_and_path(session: &mut Session<'_>, script: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run_now(script).unwrap();
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
        .run_now("DEFINE VECTOR crowd DIMENSION 3 DISTANCE euclidean QUANTIZED;")
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
        .run_now(
            "DEFINE VECTOR full DIMENSION 3 DISTANCE euclidean;\n\
             DEFINE VECTOR coded DIMENSION 3 DISTANCE euclidean QUANTIZED;",
        )
        .unwrap();
    crowded(&mut session, "full");
    crowded(&mut session, "coded");
    session
        .run_now("REBUILD INDEX vector ON full; REBUILD INDEX vector ON coded;")
        .unwrap();

    let info = |session: &mut Session<'_>, name: &str| {
        let outcomes = session
            .run_now(&format!("INFO FOR VECTOR {name};"))
            .unwrap();
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
    session.run_now("DEFINE COLLECTION people;").unwrap();
    assert!(
        session
            .run_now("DEFINE INDEX by_city ON people FIELDS city QUANTIZED;")
            .is_err()
    );
}

#[test]
fn a_partitioned_index_is_built_and_serves_approximate_reads() {
    // ADR-0141: the kind T2.1 refused by name now builds, probes and
    // reports. The codes choose who is tried; the full vectors decide the
    // order — the same division of labour the quantized graph kind states.
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE VECTOR crowd DIMENSION 3 DISTANCE euclidean PARTITIONS 4;")
        .unwrap();
    crowded(&mut session, "crowd");
    session.run_now("REBUILD INDEX vector ON crowd;").unwrap();
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
        "the probes chose the candidates, the full vectors the order"
    );

    let outcomes = session.run_now("INFO FOR VECTOR crowd;").unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        fields.get("partitions"),
        Some(&Value::Number(Number::Integer(4)))
    );
    assert_eq!(fields.get("quantized"), Some(&Value::Bool(false)));
    // The build measured recall, and a three-component vector codes to one
    // byte — the footprint the kind exists for.
    let recall = match fields.get("recall") {
        Some(Value::Object(held)) => held,
        other => panic!("recall was {other:?}"),
    };
    assert!(
        matches!(recall.get("recall"), Some(Value::Number(Number::Integer(p))) if *p >= 90),
        "{recall:?}"
    );
    match fields.get("vector_bytes") {
        // One code byte plus its two-byte header: the footprint the kind
        // exists for, against twenty-eight bytes a full node spends.
        Some(Value::Number(Number::Integer(held))) => assert_eq!(*held, 3, "{fields:?}"),
        other => panic!("vector_bytes was {other:?}"),
    }
}

#[test]
fn a_partitioned_index_maintains_codes_on_writes() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE VECTOR crowd DIMENSION 3 DISTANCE euclidean PARTITIONS 4;")
        .unwrap();
    crowded(&mut session, "crowd");
    session.run_now("REBUILD INDEX vector ON crowd;").unwrap();
    // A record written after the build is assigned and coded on the write,
    // with no rebuild: it answers an approximate read.
    session
        .run_now("CREATE crowd:99 = { vector: [0.0, 1000.0, 500.95] };")
        .unwrap();
    let (walked, _) = ids_and_path(
        &mut session,
        "SELECT * FROM crowd ORDER BY vector::euclidean(vector, [0.0, 1000.0, 500.93]) LIMIT 1 APPROXIMATE;",
    );
    assert_eq!(walked, vec![RecordId::Int(99)]);
    // A moved record leaves no code behind: the deleted record cannot
    // surface, and the updated one still answers.
    session
        .run_now("UPDATE crowd:99 SET vector = [0.0, 1000.0, 502.5];")
        .unwrap();
    let (walked, _) = ids_and_path(
        &mut session,
        "SELECT * FROM crowd ORDER BY vector::euclidean(vector, [0.0, 1000.0, 500.93]) LIMIT 1 APPROXIMATE;",
    );
    assert_ne!(walked, vec![RecordId::Int(99)]);
    session.run_now("DELETE crowd:99;").unwrap();
    let (walked, _) = ids_and_path(
        &mut session,
        "SELECT * FROM crowd ORDER BY vector::euclidean(vector, [0.0, 1000.0, 502.5]) LIMIT 1 APPROXIMATE;",
    );
    assert_ne!(walked, vec![RecordId::Int(99)], "the dead record is gone");
}

#[test]
fn a_partitioned_index_serves_a_filtered_read() {
    // The measured 1%-selectivity collapse of the graph walk is the case the
    // kind exists for: the probe reads only the partitions it probes, and a
    // record the predicate rejects costs bytes, never a distance (ADR-0141
    // D3). A short answer declines to the exact path, so what is asserted is
    // the answer itself.
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE VECTOR crowd DIMENSION 3 DISTANCE euclidean PARTITIONS 4;
             DEFINE FIELD kind ON crowd TYPE string;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 0..40 {
        let third = 500.0 + f64::from(n) * 0.1;
        let kind = if n == 10 { "wanted" } else { "other" };
        script.push_str(&format!(
            "CREATE crowd:{n} = {{ vector: [0.0, 1000.0, {third:.1}], kind: \"{kind}\" }};\n"
        ));
    }
    session.run_now(&script).unwrap();
    session.run_now("REBUILD INDEX vector ON crowd;").unwrap();
    let (served, _) = ids_and_path(
        &mut session,
        "SELECT * FROM crowd WHERE kind = \"wanted\" ORDER BY vector::euclidean(vector, [0.0, 1000.0, 500.93]) LIMIT 1 APPROXIMATE;",
    );
    assert_eq!(served, vec![RecordId::Int(10)]);
}

#[test]
fn a_store_below_format_seventeen_refuses_a_partitioned_index_and_keeps_its_format() {
    // ADR-0118's contract for the partitioned kind (ADR-0141): a store an
    // older build wrote keeps its format until it is finalized, and an index
    // whose entries that build cannot read is refused rather than moving the
    // store — the same row the containment index holds, format 17 for kinds
    // 0x49 and 0x4a.
    use tessari_encoding::{FormatVersion, FormatVersionKey, StoreValue};
    use tessari_kv::WriteBatch;

    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    drop(store);
    // A store this build opens as is, one format below the partitioned kind.
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(16).encode(),
        ))
        .now()
        .unwrap();
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d;\n\
             DEFINE COLLECTION notes;",
        )
        .unwrap();
    let refused = session
        .run_now("DEFINE INDEX by_e ON notes FIELDS e VECTOR cosine PARTITIONS 8;")
        .unwrap_err();
    assert!(
        matches!(
            refused,
            tessari_session::Error::FormatNotFinalized {
                needs: 17,
                holds: 16,
                ..
            }
        ),
        "{refused:?}"
    );
    let stamp = backend
        .get(FormatVersionKey::keyspace(), &FormatVersionKey.encode())
        .now()
        .unwrap()
        .map(|held| FormatVersion::decode(held.as_slice()).unwrap().get());
    assert_eq!(stamp, Some(16), "a refused define moved the format");
}
