//! `REBUILD INDEX` — making an index's entries what its table's rows imply.
//!
//! # What is being asserted, and what deliberately is not
//!
//! A rebuild is repair work, and repair work is judged by what is true
//! afterwards rather than by whether it ran. So every test here looks at the
//! store: entries that describe rows nobody holds any more are gone, a search
//! index's statistics count each document once, a unique index still refuses a
//! duplicate.
//!
//! What is **not** asserted is that a rebuild changes an answer. It must not.
//! An index changes the cost of a question and never its answer — the one
//! exception in this store is the vector graph, which a statement has to ask for
//! by name — so a rebuild is invisible to every read except in how well the
//! approximate one does.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_encoding::{KeyKind, StoreKey, StoreValue, VectorNode, VectorNodeKey};
use bgv_db_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use bgv_db_session::{Error, Session};
use bgv_db_storage::Store;

/// A store, and the backend handle underneath it.
///
/// The handle is kept because what a rebuild leaves behind is a fact about the
/// keys, and a session cannot see keys. Holding a second reference is how the
/// storage crate's own sweep test looks at its entries, and it is the same
/// answer here.
fn store() -> (Store, Arc<dyn KvBackend>) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (store, backend)
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE papers; USE DATABASE papers;\n\
             DEFINE TABLE notes;",
        )
        .unwrap();
    session
}

/// How many keys the index keyspace holds under one key kind.
///
/// Deliberately over the whole keyspace rather than one index's prefix: a test
/// that counted only the prefix it expected could not notice entries left under
/// a prefix nobody looks at any more.
fn keys_of(backend: &Arc<dyn KvBackend>, kind: KeyKind) -> usize {
    let request = ScanRequest {
        keyspace: kind.keyspace(),
        range: KeyRange::prefix(&[kind.tag()]),
        direction: ScanDirection::Forward,
        limit: None,
    };
    backend.scan(&request).unwrap().len()
}

#[test]
fn a_rebuild_leaves_the_same_entries_a_first_build_would_have_written() {
    // The base case, and the one that says a rebuild is not a second build
    // stacked on the first: the store holds the same number of entries
    // afterwards, not twice as many.
    let (store, backend) = store();
    let mut session = ready(&store);
    for n in 0..20 {
        session
            .run(&format!("CREATE notes:{n} = {{ author: 'a{n}' }};"))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_author ON notes FIELDS author;")
        .unwrap();
    let before = keys_of(&backend, KeyKind::SecondaryIndex);
    assert_eq!(before, 20);

    session.run("REBUILD INDEX by_author ON notes;").unwrap();
    assert_eq!(keys_of(&backend, KeyKind::SecondaryIndex), before);
}

#[test]
fn a_rebuild_answers_the_same_question_the_same_way() {
    // An index changes cost, never an answer — including across a rebuild.
    let (store, _backend) = store();
    let mut session = ready(&store);
    for n in 0..10 {
        session
            .run(&format!(
                "CREATE notes:{n} = {{ author: 'ada', page: {n} }};"
            ))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_author ON notes FIELDS author;")
        .unwrap();

    let before = session
        .run("SELECT * FROM notes WHERE author = 'ada';")
        .unwrap()[0]
        .records()
        .unwrap()
        .len();
    session.run("REBUILD INDEX by_author ON notes;").unwrap();
    let after = session
        .run("SELECT * FROM notes WHERE author = 'ada';")
        .unwrap()[0]
        .records()
        .unwrap()
        .len();
    assert_eq!(before, 10);
    assert_eq!(after, before);
}

#[test]
fn a_rebuild_counts_a_searchable_document_once_and_not_twice() {
    // The failure a rebuild would have introduced if a build were additive: a
    // search index's collection statistics are a running total kept by deltas,
    // and a score measures a document *against the collection*. Doubling the
    // collection is not an error anybody sees — the documents still come back,
    // ranked by a number that is quietly wrong, for ever.
    //
    // So the assertion is the score itself, which is the only thing those
    // statistics are for.
    let (store, backend) = store();
    let mut session = ready(&store);
    session
        .run("DEFINE ANALYZER plain FILTERS lowercase;")
        .unwrap();
    session
        .run("DEFINE FIELD body ON notes TYPE string ANALYZER plain;")
        .unwrap();
    // Different lengths on purpose: the average document length is one of the
    // statistics, so documents of one size would hide half of a drift.
    for n in 0..8 {
        let padding = "word ".repeat(usize::try_from(n).unwrap());
        session
            .run(&format!(
                "CREATE notes:{n} = {{ body: 'salary spreadsheet {padding}' }};"
            ))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    let scores = |session: &mut Session<'_>| {
        session
            .run("SELECT search::score(body, 'salary') AS relevance FROM notes;")
            .unwrap()[0]
            .records()
            .unwrap()
            .iter()
            .map(|(id, value)| format!("{id}={value:?}"))
            .collect::<Vec<_>>()
    };
    let before = scores(&mut session);
    let postings = keys_of(&backend, KeyKind::Posting);
    assert!(postings > 0, "nothing was indexed, so nothing is proved");

    session.run("REBUILD INDEX by_body ON notes;").unwrap();

    assert_eq!(
        keys_of(&backend, KeyKind::Posting),
        postings,
        "the rebuild left the first build's postings behind"
    );
    assert_eq!(
        scores(&mut session),
        before,
        "the scores moved, so the collection statistics moved"
    );
}

#[test]
fn a_rebuild_of_a_unique_index_does_not_collide_with_its_own_entries() {
    // The trap in doing this by re-running the build: every value the index
    // already holds is claimed a second time. If the claim set were shared with
    // the entries just cleared, the rebuild would refuse itself.
    let (store, backend) = store();
    let mut session = ready(&store);
    for n in 0..5 {
        session
            .run(&format!("CREATE notes:{n} = {{ email: 'a{n}@x' }};"))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_email ON notes FIELDS email UNIQUE;")
        .unwrap();
    session.run("REBUILD INDEX by_email ON notes;").unwrap();
    assert_eq!(keys_of(&backend, KeyKind::UniqueIndex), 5);

    // And the constraint is still a constraint afterwards.
    let refused = session.run("CREATE notes:99 = { email: 'a1@x' };");
    assert!(
        matches!(refused, Err(Error::Store(_))),
        "the unique index stopped refusing: {refused:?}"
    );
}

#[test]
fn a_rebuild_forgets_the_rows_that_have_gone() {
    // What churn leaves behind is the whole reason this statement exists. For an
    // ordinary index the entries are removed as the records go, so this asserts
    // the floor rather than the interesting case: after a rebuild there is
    // nothing describing a row that is not there.
    let (store, backend) = store();
    let mut session = ready(&store);
    for n in 0..12 {
        session
            .run(&format!("CREATE notes:{n} = {{ author: 'a{n}' }};"))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_author ON notes FIELDS author;")
        .unwrap();
    for n in 0..6 {
        session.run(&format!("DELETE notes:{n};")).unwrap();
    }
    session.run("REBUILD INDEX by_author ON notes;").unwrap();
    assert_eq!(keys_of(&backend, KeyKind::SecondaryIndex), 6);
}

#[test]
fn rebuilding_and_writing_in_one_transaction_ends_with_both() {
    // A rebuild is a mutation like any other, so it is atomic with whatever
    // shares its transaction — and the rows it indexes include the ones that
    // transaction just wrote, not only the ones that were committed before it.
    let (store, backend) = store();
    let mut session = ready(&store);
    for n in 0..4 {
        session
            .run(&format!("CREATE notes:{n} = {{ author: 'a{n}' }};"))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_author ON notes FIELDS author;")
        .unwrap();

    session
        .run(
            "BEGIN;\n\
             CREATE notes:100 = { author: 'later' };\n\
             REBUILD INDEX by_author ON notes;\n\
             COMMIT;",
        )
        .unwrap();
    assert_eq!(keys_of(&backend, KeyKind::SecondaryIndex), 5);

    let found = session
        .run("SELECT * FROM notes WHERE author = 'later';")
        .unwrap()[0]
        .records()
        .unwrap()
        .len();
    assert_eq!(
        found, 1,
        "the row written beside the rebuild is not indexed"
    );
}

#[test]
fn a_rebuild_needs_the_index_to_exist() {
    let (store, _backend) = store();
    let mut session = ready(&store);
    let refused = session.run("REBUILD INDEX nothing ON notes;");
    assert!(
        matches!(
            refused,
            Err(Error::Unknown {
                entity: "index",
                ..
            })
        ),
        "{refused:?}"
    );
}

/// Every edge of a vector index that points at a record the index no longer
/// holds.
///
/// The measure of what churn does to this store's one approximate index.
/// Removing a record takes its node and the edges *out* of it; the edges *into*
/// it are left, because finding them means reading every node that might point
/// here. Nothing goes wrong that a reader can see — a candidate that does not
/// resolve produces no row — and the search quietly gets worse.
fn dangling_edges(backend: &Arc<dyn KvBackend>) -> usize {
    let request = ScanRequest {
        keyspace: KeyKind::VectorNode.keyspace(),
        range: KeyRange::prefix(&[KeyKind::VectorNode.tag()]),
        direction: ScanDirection::Forward,
        limit: None,
    };
    let found = backend.scan(&request).unwrap();
    let present: Vec<_> = found
        .iter()
        .map(|(key, _)| VectorNodeKey::decode(key.as_slice()).unwrap().id)
        .collect();
    found
        .iter()
        .map(|(_, value)| VectorNode::decode(value.as_slice()).unwrap())
        .flat_map(|node| node.neighbours)
        .filter(|id| !present.contains(id))
        .count()
}

#[test]
fn rebuilding_a_vector_index_takes_back_the_edges_churn_left() {
    // The row the engine gap list graded worst, and the reason this statement
    // exists. Everything else in this store refuses a question it cannot answer;
    // a churned vector index answers it *worse and worse*, and says nothing.
    let (store, backend) = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE points;").unwrap();
    for n in 0..200 {
        let x = f64::from(n) * 0.5;
        session
            .run(&format!("CREATE points:{n} = {{ at: [{x:.2}, 0.0] }};"))
            .unwrap();
    }
    session
        .run("DEFINE INDEX by_at ON points FIELDS at VECTOR euclidean;")
        .unwrap();
    assert_eq!(dangling_edges(&backend), 0, "a fresh index already dangles");

    // Half of them go, taken from across the line rather than off one end: what
    // is being damaged is the links, not the presence of a region.
    for n in (1..200).step_by(2) {
        session.run(&format!("DELETE points:{n};")).unwrap();
    }
    let churned = dangling_edges(&backend);
    assert!(churned > 0, "the fixture did not churn the index");

    session.run("REBUILD INDEX by_at ON points;").unwrap();
    assert_eq!(
        dangling_edges(&backend),
        0,
        "the rebuild left {churned} dangling edges behind"
    );

    // And the index still answers — approximately, because that is the one
    // question in this store where a statement has to say so.
    let found = session
        .run("SELECT * FROM points ORDER BY vector::euclidean(at, [1.0, 0.0]) LIMIT 3 APPROXIMATE;")
        .unwrap()[0]
        .records()
        .unwrap()
        .len();
    assert_eq!(found, 3);
}
