//! What a read of a vector store answers, and which path answered it.
//!
//! # The criterion
//!
//! **`APPROXIMATE` remains the only way to obtain an approximate answer.** The
//! word is the caller's, per statement, and nothing in a declaration may take it
//! over — a store that became approximate by virtue of being declared would turn
//! two identical-looking reads into two different contracts, and the difference
//! would be invisible in both of them.
//!
//! `notes.rs` already holds this for a vector index declared the long way. What
//! is new is the **declared store**: a second doorway that did not exist when
//! that file was written, and the one a caller who has only ever typed
//! `DEFINE VECTOR` will use.
//!
//! # Why the assertions are about the path and the note rather than the answer
//!
//! Over any dataset small enough to write in a test the graph and the scan agree,
//! so an answer comparison would pass whichever path ran and would prove nothing
//! — the vacuous shape this project has recorded before. The access path and the
//! note are the store's **own statement** of which path answered, and they are
//! asserted in both directions: a read without the word must report the scan, and
//! a read with it must report the graph. Remove the gate and the first fails;
//! break the graph's selection and the second does. Neither is satisfiable by
//! "the scan always runs".

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE library; USE DATABASE library;
";

/// Forty points on a line, written into whatever `embeddings` already is.
fn fill(session: &mut Session<'_>) {
    let mut script = String::new();
    for n in 0..40_u32 {
        script.push_str(&format!(
            "CREATE embeddings:{n} = {{ vector: [{n}.0, 0.0] }};\n"
        ));
    }
    session.run(&script).unwrap();
}

/// A store declared by the word, filled.
fn through_the_store(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{PLACE}DEFINE VECTOR embeddings DIMENSION 2 DISTANCE euclidean;"
        ))
        .unwrap();
    fill(&mut session);
    session
}

/// The same thing said the long way, filled.
fn through_the_field(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{PLACE}DEFINE COLLECTION embeddings;\n\
             DEFINE FIELD vector ON embeddings TYPE vector<2> REQUIRED;\n\
             DEFINE INDEX vector ON embeddings FIELDS vector VECTOR euclidean;"
        ))
        .unwrap();
    fill(&mut session);
    session
}

const NEAR: &str = "SELECT * FROM embeddings \
                    ORDER BY vector::euclidean(vector, [0.0, 0.0]) LIMIT 5";

/// The records a read answered with, its notes, and the path that served it.
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

#[test]
fn a_declared_stores_default_read_is_exact() {
    // The criterion, in one test. The store carries its index by construction —
    // there is no arrangement in which it does not — so if a declaration were
    // ever going to make a read approximate on its own, this is the read it
    // would happen on.
    let held = store();
    let mut session = through_the_store(&held);

    let (records, notes, path) = answered(&mut session, &format!("{NEAR};"));
    // The scan, and not `Ordered`: `Ordered` is a bounded ordered read an index
    // served, and a distance over a stored vector is computed per record however
    // the read is written. The exactness is the point, and the path is how the
    // store says which one it took.
    assert_eq!(path, AccessPath::Scan, "{path:?}");
    assert!(notes.is_empty(), "an exact answer raised {notes:?}");
    assert_eq!(records.len(), 5);
}

#[test]
fn the_word_is_what_reaches_the_graph_and_the_note_travels_with_it() {
    // The other half, and the reason the test above is not vacuous: over this
    // data the two paths return the same five records, so only the path and the
    // note distinguish them. Both are asserted, in both directions.
    let held = store();
    let mut session = through_the_store(&held);

    let (exact, _, _) = answered(&mut session, &format!("{NEAR};"));
    let (near, notes, path) = answered(&mut session, &format!("{NEAR} APPROXIMATE;"));

    assert_eq!(path, AccessPath::Approximate, "{path:?}");
    assert_eq!(notes, vec![Note::Approximate]);
    // Approximate is a statement about the guarantee rather than a licence to
    // answer badly: over forty points on a line the walk finds the same five.
    assert_eq!(near, exact);
}

#[test]
fn both_doorways_answer_the_same_read_the_same_way() {
    // The store and the three statements it stands for are one arrangement, so
    // the path, the notes and the records must agree on both reads. A difference
    // here is the two doorways having become two implementations — the same
    // property `vector_store.rs` asserts for the refusal, asserted for the read.
    let one = store();
    let two = store();
    let mut from_store = through_the_store(&one);
    let mut from_field = through_the_field(&two);

    for read in [format!("{NEAR};"), format!("{NEAR} APPROXIMATE;")] {
        assert_eq!(
            answered(&mut from_store, &read),
            answered(&mut from_field, &read),
            "{read}"
        );
    }
}

#[test]
fn a_read_asking_a_distance_the_store_was_not_built_for_gets_the_exact_answer() {
    // The store's graph was built with `euclidean`. Cosine measures an angle and
    // euclidean a separation, and for vectors nobody normalised they rank
    // differently — so serving a cosine read from a euclidean graph would return
    // plausible neighbours that are not the nearest. The word is present and the
    // index still declines, which is the point: `APPROXIMATE` is permission, not
    // instruction.
    let held = store();
    let mut session = through_the_store(&held);

    let (_, _, path) = answered(
        &mut session,
        "SELECT * FROM embeddings \
         ORDER BY vector::cosine(vector, [1.0, 0.0]) LIMIT 5 APPROXIMATE;",
    );
    assert_ne!(path, AccessPath::Approximate, "{path:?}");
}

#[test]
fn the_word_alone_does_not_reach_the_graph_when_the_read_is_a_different_shape() {
    // Each of these says `APPROXIMATE` and none of them is a read a graph
    // answers. They are asserted together because the gate is one condition set
    // and a fall-back that survived only three of five would be found by whoever
    // wrote the fourth read, in production.
    let held = store();
    let mut session = through_the_store(&held);

    for read in [
        // No bound: the read wants every record and a walk has nothing to cut.
        "SELECT * FROM embeddings ORDER BY vector::euclidean(vector, [0.0, 0.0]) APPROXIMATE;",
        // Descending: a distance orders ascending, so this asks for the least
        // similar — a query the language allows and a graph does not answer.
        "SELECT * FROM embeddings \
         ORDER BY vector::euclidean(vector, [0.0, 0.0]) DESC LIMIT 5 APPROXIMATE;",
        // `dot` grows with similarity, so ordering by it ascending asks for the
        // least similar too, by the other route.
        "SELECT * FROM embeddings \
         ORDER BY vector::dot(vector, [1.0, 0.0]) LIMIT 5 APPROXIMATE;",
        // A second key orders records the graph never ranked.
        "SELECT * FROM embeddings \
         ORDER BY vector::euclidean(vector, [0.0, 0.0]), vector LIMIT 5 APPROXIMATE;",
    ] {
        let (_, _, path) = answered(&mut session, read);
        assert_ne!(path, AccessPath::Approximate, "{read}");
    }
}
