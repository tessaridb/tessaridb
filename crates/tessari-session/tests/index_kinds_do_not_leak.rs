//! Declaring an index never removes rows from an answer.
//!
//! The store's governing rule is that an index changes what a read **costs** and
//! never what it **answers**. Every other index test asserts that for the kind it
//! is about. This one asserts it across kinds, which is where it was false twice.
//!
//! Each index kind writes a different key: an ordered index writes the indexed
//! value, a search index writes terms, a vector index writes graph nodes, a
//! spatial index writes the cells covering a geometry. A planner that decides
//! whether an index can serve an equality by asking which kinds to **skip** has
//! to name every kind, and a kind added later is then silently admitted — the
//! plan says `Index`, the read looks a value up in a keyspace that is not keyed
//! by values, and it answers with **fewer rows and no error**.
//!
//! Both instances were found by running the same query twice, once before the
//! index existed and once after, and comparing. That is what this file does, for
//! every kind, and it is why the comparison is against the **scan's own answer**
//! rather than against a number written down here: a number would have to be
//! re-derived by hand for every new case, and the scan is the definition.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE atlas; USE NAMESPACE atlas;\n\
             DEFINE DATABASE world; USE DATABASE world;\n\
             DEFINE TABLE notes SCHEMALESS;",
        )
        .unwrap();
    session
}

/// How many records one statement answered with.
fn rows(session: &mut Session<'_>, statement: &str) -> usize {
    match session.run(statement).unwrap().pop() {
        Some(Outcome::Records { records, .. }) => records.len(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// Run `query` before and after `define`, and require the same answer.
fn unchanged_by(populate: &str, query: &str, define: &str) {
    let store = store();
    let mut session = ready(&store);
    session.run(populate).unwrap();

    let scanned = rows(&mut session, query);
    assert!(
        scanned > 0,
        "the case proves nothing unless the scan finds something: {query}"
    );

    session.run(define).unwrap();
    let indexed = rows(&mut session, query);
    assert_eq!(
        indexed, scanned,
        "`{define}` changed the answer to `{query}` from {scanned} row(s) to \
         {indexed} — an index may change what a read costs, never what it says"
    );
}

#[test]
fn a_spatial_index_does_not_capture_an_equality_on_its_field() {
    unchanged_by(
        "CREATE notes:1 = { at: geometry { type: 'Point', coordinates: [2.35, 48.85] } };",
        "SELECT * FROM notes WHERE at = geometry { type: 'Point', coordinates: [2.35, 48.85] };",
        "DEFINE INDEX by_at ON notes FIELDS at SPATIAL;",
    );
}

#[test]
fn a_vector_index_does_not_capture_an_equality_on_its_field() {
    // Pre-existing, and shipped for as long as vector indexes have. Found while
    // fixing the spatial one, by asking whether the same question had the same
    // answer — which it did.
    unchanged_by(
        "CREATE notes:1 = { embedding: [1.0, 2.0] };",
        "SELECT * FROM notes WHERE embedding = [1.0, 2.0];",
        "DEFINE INDEX by_vec ON notes FIELDS embedding VECTOR cosine;",
    );
}

#[test]
fn a_search_index_does_not_capture_an_equality_on_its_field() {
    // This one was already right — a search index is excluded by name at every
    // site, because it was the first kind that needed excluding. It is here so
    // the file states the rule for every kind rather than for the two that broke
    // it, and so a future change that unifies the sites cannot quietly drop it.
    unchanged_by(
        "DEFINE ANALYZER simple FILTERS lowercase;\n\
         DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
         CREATE notes:1 = { body: 'a whole sentence' };",
        "SELECT * FROM notes WHERE body = 'a whole sentence';",
        "DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
    );
}

#[test]
fn a_spatial_index_does_not_capture_a_range_on_its_field() {
    // The other door into the same room: `ranged` enumerated the kinds it skips
    // exactly as `serving` did, so both had to be fixed and both need a case.
    unchanged_by(
        "CREATE notes:1 = { n: 5, at: geometry { type: 'Point', coordinates: [0, 0] } };",
        "SELECT * FROM notes WHERE at > 1;",
        "DEFINE INDEX by_at ON notes FIELDS at SPATIAL;",
    );
}

#[test]
fn an_ordered_index_still_serves_the_equality_it_is_for() {
    // The fix is a predicate that answers "no" by default, so the test that
    // matters most is that it still answers "yes" for the kind it should — a
    // predicate excluding everything would satisfy every assertion above.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE notes:1 = { city: 'paris' }; CREATE notes:2 = { city: 'berlin' };")
        .unwrap();
    session
        .run("DEFINE INDEX by_city ON notes FIELDS city;")
        .unwrap();
    let answered = session
        .run("SELECT * FROM notes WHERE city = 'paris';")
        .unwrap();
    match answered.first() {
        Some(Outcome::Records { records, plan, .. }) => {
            assert_eq!(records.len(), 1);
            assert_eq!(
                format!("{:?}", plan.access),
                "Index",
                "an ordered index must still be chosen for its own equality"
            );
        }
        other => panic!("expected records, got {other:?}"),
    }
}
