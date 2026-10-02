//! A filtered nearest-neighbour read: `WHERE … ORDER BY vector::… LIMIT k
//! APPROXIMATE`.
//!
//! Until 0.22 the graph answered only a read with no `WHERE`, and a condition
//! sent the read to the exact scan whatever it asked for. What is asserted
//! here is the contract of the filtered walk: the graph serves the read and says
//! so, every answer passes the WHOLE condition, the page is full whenever enough
//! records match, and a walk that cannot fill it gives the read back to the exact
//! path with a note — never a short page.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Points along a line at `x = n`, each carrying its parity and a rare marker.
fn ready(store: &Store, count: i64) -> Session<'_> {
    let mut session = Session::new(store);
    let mut script = String::from(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
         DEFINE DATABASE shop; USE DATABASE shop;\n\
         DEFINE COLLECTION items;\n",
    );
    for n in 0..count {
        let even = n.rem_euclid(2) == 0;
        let rare = n == 37;
        script.push_str(&format!(
            "CREATE items:{n} = {{ at: [{n}.0, 0.0], even: {even}, rare: {rare} }};\n"
        ));
    }
    script.push_str("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;\n");
    session.run(&script).unwrap();
    session
}

fn answered(session: &mut Session<'_>, script: &str) -> (Vec<RecordId>, Vec<Note>, AccessPath) {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Records {
        records,
        plan,
        notes,
        ..
    }) = outcomes.last()
    else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        notes.clone(),
        plan.access,
    )
}

const EVEN_NEAR_ZERO: &str =
    "SELECT * FROM items WHERE even = true ORDER BY vector::euclidean(at, [0.5, 0.0]) LIMIT 3";

#[test]
fn a_filtered_approximate_read_is_served_by_the_graph_and_says_so() {
    let store = store();
    let mut session = ready(&store, 60);
    let (ids, notes, path) = answered(&mut session, &format!("{EVEN_NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Approximate);
    assert!(notes.contains(&Note::Approximate), "{notes:?}");
    // Sixty points on a line, a budget above them: the walk reaches every
    // record, so its answer is the exact one — pinned to content, not only to
    // agreement with another derived read.
    assert_eq!(
        ids,
        vec![RecordId::Int(0), RecordId::Int(2), RecordId::Int(4)]
    );
}

#[test]
fn without_the_word_a_filtered_read_stays_exact() {
    let store = store();
    let mut session = ready(&store, 60);
    let (ids, notes, path) = answered(&mut session, &format!("{EVEN_NEAR_ZERO};"));
    assert_eq!(path, AccessPath::Scan);
    assert!(!notes.contains(&Note::Approximate), "{notes:?}");
    assert_eq!(
        ids,
        vec![RecordId::Int(0), RecordId::Int(2), RecordId::Int(4)]
    );
}

#[test]
fn every_answer_passes_the_whole_condition() {
    let store = store();
    let mut session = ready(&store, 400);
    let outcomes = session
        .run(
            "SELECT * FROM items WHERE even = true AND at[0] > 100 \
             ORDER BY vector::euclidean(at, [50.0, 0.0]) LIMIT 5 APPROXIMATE;",
        )
        .unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(plan.access, AccessPath::Approximate);
    assert_eq!(records.len(), 5);
    for (id, record) in records {
        let Value::Object(fields) = record else {
            panic!("{record:?}");
        };
        assert_eq!(fields.get("even"), Some(&Value::Bool(true)), "{id:?}");
        let RecordId::Int(n) = id else {
            panic!("{id:?}")
        };
        assert!(*n > 100, "{id:?}");
    }
}

#[test]
fn a_walk_that_cannot_fill_the_page_gives_the_read_to_the_exact_path() {
    // One record in four hundred matches: the walk runs out of graph with one
    // admitted record, and the read is answered exactly, with a note saying the
    // approximate path was given up — never a page the graph could not fill.
    let store = store();
    let mut session = ready(&store, 400);
    let (ids, notes, path) = answered(
        &mut session,
        "SELECT * FROM items WHERE rare = true \
         ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 3 APPROXIMATE;",
    );
    assert_eq!(path, AccessPath::Scan);
    assert_eq!(ids, vec![RecordId::Int(37)]);
    assert!(
        notes.contains(&Note::FellBack {
            from: AccessPath::Approximate,
            to: AccessPath::Scan,
        }),
        "{notes:?}"
    );
}

#[test]
fn a_record_written_in_this_transaction_keeps_the_filtered_read_exact() {
    let store = store();
    let mut session = ready(&store, 60);
    let outcomes = session
        .run(&format!(
            "BEGIN; CREATE items:900 = {{ at: [0.4, 0.0], even: true, rare: false }};\n\
             {EVEN_NEAR_ZERO} APPROXIMATE; COMMIT;"
        ))
        .unwrap();
    let read = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            Outcome::Records { records, plan, .. } => Some((records, plan.access)),
            _ => None,
        })
        .unwrap();
    assert_eq!(read.1, AccessPath::Scan);
    assert_eq!(read.0[0].0, RecordId::Int(900));
}

#[test]
fn a_condition_an_index_narrows_to_few_records_is_answered_exactly() {
    // Thirty candidates from an index on the condition cost no more than the
    // walk would and give the exact answer, so the read keeps the index path —
    // and an `APPROXIMATE` read answered exactly carries no approximate note.
    let store = store();
    let mut session = ready(&store, 60);
    session
        .run("DEFINE INDEX by_even ON items FIELDS even;")
        .unwrap();
    let (ids, notes, path) = answered(&mut session, &format!("{EVEN_NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Index);
    assert!(!notes.contains(&Note::Approximate), "{notes:?}");
    assert_eq!(
        ids,
        vec![RecordId::Int(0), RecordId::Int(2), RecordId::Int(4)]
    );
}

#[test]
fn explain_names_the_graph_for_a_filtered_approximate_read() {
    let store = store();
    let mut session = ready(&store, 60);
    let outcomes = session
        .run(&format!("EXPLAIN {EVEN_NEAR_ZERO} APPROXIMATE;"))
        .unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        plan.get("access"),
        Some(&Value::String("approximate".into()))
    );
    assert_eq!(plan.get("index"), Some(&Value::String("by_at".into())));
}
