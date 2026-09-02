//! The one index in this store whose answer differs from the scan's.
//!
//! Every other index is asserted to change the cost and not the answer. This one
//! cannot be: a navigable graph returns the neighbours a walk found and cannot
//! show it missed none without doing the scan it exists to avoid. So what is
//! asserted here is the **contract around** that:
//!
//! - silence gets the exact scan, whatever indexes exist;
//! - `APPROXIMATE` is permission, not a demand — with no index, or with one built
//!   for a different distance, the read is still exact and says `scan`;
//! - only when the statement asked *and* the index matches does the graph run.
//!
//! A test that the approximate answer equals the exact one would be asserting
//! something this index does not promise. What it promises is measured by the
//! benchmark harness, as recall, and recorded rather than invented.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION items;",
        )
        .unwrap();
    session
}

/// Points along a line, so which is nearest is obvious by inspection.
fn populate(session: &mut Session<'_>, count: i64) {
    for n in 0..count {
        let x = f64::from(i32::try_from(n).unwrap());
        session
            .run(&format!("CREATE items:{n} = {{ at: [{x:.1}, 0.0] }};"))
            .unwrap();
    }
}

/// The identities a read answered with, and the path it took.
///
/// A graph-served read reports `approximate` rather than `index`: it is the one
/// read in this store an index answers *differently* from a scan, and the word
/// that says so is the word `EXPLAIN` has always used for it.
fn read(session: &mut Session<'_>, script: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(script).unwrap();
    let path = outcomes[0].path().unwrap();
    let ids = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    (ids, path)
}

const NEAR_ZERO: &str = "SELECT * FROM items ORDER BY vector::euclidean(at, [0.5, 0.0]) LIMIT 3";

#[test]
fn without_the_word_the_index_does_not_serve_the_read() {
    // The rule the whole store is built on, held at the one place it could bend:
    // adding an index must not change what a statement answers, so a statement
    // that did not ask for an approximation does not get one.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);

    let (before, path) = read(&mut session, &format!("{NEAR_ZERO};"));
    assert_eq!(path, AccessPath::Scan);

    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();

    let (after, path) = read(&mut session, &format!("{NEAR_ZERO};"));
    assert_eq!(path, AccessPath::Scan, "the index served an unasked read");
    assert_eq!(before, after);
}

#[test]
fn with_the_word_and_no_index_the_read_is_still_exact() {
    // `APPROXIMATE` is permission and not a demand: with nothing to serve it the
    // read is exact, which is better than what was asked for, and the access
    // path is what says which happened.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);

    let (found, path) = read(&mut session, &format!("{NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Scan);
    assert_eq!(
        found,
        vec![RecordId::Int(0), RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn with_the_word_and_a_matching_index_the_graph_runs() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);
    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();

    let (found, path) = read(&mut session, &format!("{NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Approximate);
    // On forty points along a line the walk finds the exact three. That is not
    // promised in general and is not asserted as a property — it is asserted
    // here because a graph that could not manage it on data this simple would be
    // broken rather than approximate.
    assert_eq!(
        found,
        vec![RecordId::Int(0), RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn an_index_built_for_another_distance_does_not_serve_the_read() {
    // A graph whose edges were chosen by one measure approximates that measure
    // and no other, so a cosine query over a euclidean graph would return
    // plausible neighbours that are not the nearest.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);
    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR cosine;")
        .unwrap();

    let (_, path) = read(&mut session, &format!("{NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Scan);

    // And its own distance is served.
    let (_, path) = read(
        &mut session,
        "SELECT * FROM items ORDER BY vector::cosine(at, [0.5, 0.1]) LIMIT 3 APPROXIMATE;",
    );
    assert_eq!(path, AccessPath::Approximate);
}

#[test]
fn a_shape_the_graph_does_not_answer_falls_back_to_the_scan() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);
    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();

    for script in [
        // No bound: the read wants every record and a walk has nothing to cut.
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.5, 0.0]) APPROXIMATE;",
        // Descending: a distance orders ascending, and reversing it asks for the
        // furthest, which a nearest-neighbour graph does not hold.
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.5, 0.0]) DESC LIMIT 3 APPROXIMATE;",
        // The inner product grows with similarity, so ascending asks for the
        // least similar.
        "SELECT * FROM items ORDER BY vector::dot(at, [0.5, 0.0]) LIMIT 3 APPROXIMATE;",
        // A second key orders records the graph never ranked.
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.5, 0.0]), id LIMIT 3 APPROXIMATE;",
        // A sort key that is not a distance at all.
        "SELECT * FROM items ORDER BY at LIMIT 3 APPROXIMATE;",
    ] {
        let (_, path) = read(&mut session, script);
        assert_eq!(path, AccessPath::Scan, "served: {script}");
    }
}

#[test]
fn a_deleted_record_is_never_answered_with() {
    // The graph keeps edges into a removed node — finding them means reading
    // every node that might point here — so correctness rests on the same rule
    // every index read follows: a candidate is resolved at the reader's own
    // snapshot, and one that does not resolve produces no row.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);
    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();
    session.run("DELETE items:0; DELETE items:1;").unwrap();

    let (found, path) = read(&mut session, &format!("{NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Approximate);
    assert!(!found.contains(&RecordId::Int(0)), "{found:?}");
    assert!(!found.contains(&RecordId::Int(1)), "{found:?}");
    assert_eq!(found[0], RecordId::Int(2));
}

#[test]
fn a_changed_vector_moves_the_record_in_the_graph() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 40);
    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();

    // Record 39 was the furthest; move it to the origin.
    session
        .run("UPDATE items:39 = { at: [0.0, 0.0] };")
        .unwrap();
    let (found, path) = read(&mut session, &format!("{NEAR_ZERO} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Approximate);
    assert!(found.contains(&RecordId::Int(39)), "{found:?}");
}

#[test]
fn defining_the_index_after_the_rows_reaches_the_same_answer_as_before_them() {
    // The two paths into the graph — `build` over a populated table, and one
    // insert per write — must agree, or the same collection would answer
    // differently depending on when the index was declared.
    let answered = |before: bool| {
        let store = store();
        let mut session = ready(&store);
        if before {
            session
                .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
                .unwrap();
            populate(&mut session, 40);
        } else {
            populate(&mut session, 40);
            session
                .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
                .unwrap();
        }
        read(&mut session, &format!("{NEAR_ZERO} APPROXIMATE;")).0
    };
    assert_eq!(answered(true), answered(false));
}

#[test]
fn a_record_with_no_vector_is_not_in_the_graph() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 10);
    session
        .run(
            "CREATE items:100 = { name: 'no vector here' };\n\
             CREATE items:101 = { at: 'not an array' };\n\
             CREATE items:102 = { at: [] };\n\
             DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;",
        )
        .unwrap();

    let (found, path) = read(
        &mut session,
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 20 APPROXIMATE;",
    );
    assert_eq!(path, AccessPath::Approximate);
    for absent in [100_i64, 101, 102] {
        assert!(!found.contains(&RecordId::Int(absent)), "{found:?}");
    }
    assert_eq!(found.len(), 10);
}

#[test]
fn a_distance_the_store_does_not_have_is_refused_where_it_is_written() {
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR manhattan;")
        .unwrap_err();
    match refused {
        Error::NoSuchDistance { name, .. } => assert_eq!(name, "manhattan"),
        other => panic!("wrong refusal: {other}"),
    }
}

#[test]
fn one_index_kind_and_not_two() {
    // Three markers of which at most one can be true. Accepting two used to be
    // possible and the first one checked simply won, so a `UNIQUE SEARCH` index
    // was one whose uniqueness was silently ignored.
    let store = store();
    let mut session = ready(&store);
    assert!(
        session
            .run("DEFINE INDEX by_at ON items FIELDS at UNIQUE SEARCH;")
            .is_err()
    );
    assert!(
        session
            .run("DEFINE INDEX by_at ON items FIELDS at SEARCH VECTOR cosine;")
            .is_err()
    );
}

#[test]
fn approximate_and_vector_are_still_usable_as_names() {
    // Contextual, like `order`, `group` and `fetch`. A store of embeddings whose
    // records cannot have a field called `vector` would be an odd one.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE items:1 = { vector: [1.0], approximate: true };")
        .unwrap();
    let outcomes = session
        .run("SELECT vector, approximate FROM items:1;")
        .unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 1);
}
