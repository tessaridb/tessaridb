//! Whether an answer is provably the one the question names — returned, on
//! every read, whether or not it is interesting.
//!
//! # The criterion, and why a note does not satisfy it
//!
//! `Note::Approximate` already says the right thing about the one approximate
//! read this store has. What it cannot do is make a caller **unable to miss it**:
//! notes are opt-in by construction, and a caller that reads none of them gets
//! exactly the records it would have got before notes existed. An approximate
//! answer and an exact one are then the same shape, the same length, usually the
//! same records — and, to that caller, the same claim.
//!
//! The second failure is the one that outlives any single read: if exactness is
//! something an approximate path *adds*, a path added later that forgets reads as
//! exact, because absence-means-exact is a default nobody chose.
//!
//! # So these tests assert three separable things
//!
//! **That the field is always there**, including — especially — when it is
//! `true`. Make the renderer skip it when the answer is exact and
//! `an_exact_answer_still_carries_the_claim` fails, which is the whole of the
//! difference between a returned property and a note.
//!
//! **That it is right per surface**, asserted for every path a read can take
//! rather than for the one that is interesting. A test that only checked the
//! vector walk would pass on an implementation that said `false` everywhere.
//!
//! **That it cannot be forgotten.** The exhaustive match in
//! `AccessPath::exactness` is a compile-time gate a test cannot observe, so what
//! is asserted here is the property that gate protects: every path has a decided
//! answer, and exactly one of them is approximate today.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Exactness, Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

/// Text with a search index over it, and an ordinary index beside it, so one
/// fixture can be read down every path this store offers.
fn texts(indexed: bool) -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             DEFINE FIELD city ON notes TYPE string;\n\
             DEFINE INDEX by_city ON notes FIELDS city;\n\
             CREATE notes:1 = {{ body: 'Vector search over a store', city: 'Paris' }};\n\
             CREATE notes:2 = {{ body: 'A container for the analyzer', city: 'Lyon' }};\n\
             CREATE notes:3 = {{ body: 'Locking and contention', city: 'Paris' }};",
        ))
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    held
}

/// Forty points on a line, which is the only read in this store that answers
/// approximately.
fn points() -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}DEFINE VECTOR embeddings DIMENSION 2 DISTANCE euclidean;"
        ))
        .unwrap();
    let mut script = String::new();
    for n in 0..40_u32 {
        script.push_str(&format!(
            "CREATE embeddings:{n} = {{ vector: [{n}.0, 0.0] }};\n"
        ));
    }
    session.run(&script).unwrap();
    held
}

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// What one read reported: the path it took, whether it called itself exact, the
/// object it renders as, and its notes.
fn reported(session: &mut Session<'_>, read: &str) -> (AccessPath, Exactness, Value, Vec<Note>) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { plan, notes, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (plan.access, plan.exact, plan.to_value(), notes.clone())
}

/// One key of a rendered plan, or nothing when the plan does not carry it.
fn field<'a>(plan: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Object(fields) = plan else {
        panic!("a plan rendered as {plan:?}");
    };
    fields.get(key)
}

/// Every path a read can take has a decided answer, and exactly one of them is
/// approximate.
///
/// The exhaustive match this protects is a compile-time gate: a variant added to
/// `AccessPath` will not build until somebody says where it sits, and no test can
/// watch that happen. What a test *can* watch is the property the gate exists to
/// keep true — that no path is silently exact — plus the count, which is what
/// notices a later change that decided a new path the lazy way.
#[test]
fn every_access_path_has_a_decided_exactness() {
    let approximate: Vec<AccessPath> = AccessPath::ALL
        .into_iter()
        .filter(|path| !path.exactness().is_exact())
        .collect();
    assert_eq!(
        approximate,
        vec![AccessPath::Approximate],
        "the set of approximate paths moved",
    );
    for path in AccessPath::ALL {
        match path.exactness() {
            Exactness::Exact => assert!(path.exactness().reason().is_none()),
            // A `false` whose reason is empty has told a caller that it cannot
            // trust the answer and nothing about what to do instead, which is
            // most of the value of saying so at all.
            Exactness::Approximate(why) => {
                assert!(
                    !why.is_empty(),
                    "{path:?} is approximate for no stated reason"
                );
            }
        }
    }
}

/// The renderer writes `exact` even when it is `true`, and that is the whole
/// difference between a returned property and a note.
///
/// Falsifiable in one edit: make `Plan::to_value` insert the key only when the
/// answer is approximate — the shape every other field of a plan follows — and
/// this fails. It is the test that stops the field from quietly becoming a note
/// under another name.
#[test]
fn an_exact_answer_still_carries_the_claim() {
    let held = texts(false);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let (path, exact, rendered, notes) = reported(&mut session, "SELECT * FROM notes;");

    assert_eq!(path, AccessPath::Scan);
    assert_eq!(exact, Exactness::Exact);
    assert_eq!(
        field(&rendered, "exact"),
        Some(&Value::Bool(true)),
        "an exact plan rendered as {rendered:?}",
    );
    assert!(
        field(&rendered, "inexact").is_none(),
        "an exact answer gave a reason for being inexact",
    );
    assert!(notes.is_empty(), "an exact read raised {notes:?}");
}

/// Every path this store reaches records by, read down one fixture, reports
/// itself exact.
///
/// Written as a table rather than as one test per path so that the assertion is
/// visibly *every* path and not the ones somebody remembered. The search
/// surfaces this band added are the point of the wave and are named explicitly:
/// "fuzzy" reads like "approximate" and is not — a capped expansion is declined
/// as a candidate and the scan answers, so both operators reach provably the
/// records they name.
#[test]
fn every_exact_surface_says_so() {
    for indexed in [false, true] {
        let held = texts(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        for read in [
            "SELECT * FROM notes;",
            "SELECT * FROM notes:1;",
            "SELECT * FROM notes WHERE city = 'Paris';",
            "SELECT * FROM notes ORDER BY city LIMIT 2;",
            "SELECT * FROM notes WHERE body MATCHES 'vector';",
            "SELECT * FROM notes WHERE body MATCHES PREFIX 'vect';",
            "SELECT * FROM notes WHERE body MATCHES FUZZY 'vectr';",
            "SELECT * FROM (SELECT * FROM notes LIMIT 2);",
        ] {
            let (path, exact, rendered, _) = reported(&mut session, read);
            assert_eq!(
                exact,
                Exactness::Exact,
                "{read} (indexed={indexed}) reported {exact:?} via {path:?}",
            );
            assert_eq!(
                field(&rendered, "exact"),
                Some(&Value::Bool(true)),
                "{read} (indexed={indexed}) rendered {rendered:?}",
            );
        }
    }
}

/// The graph walk says it is not exact, and says why.
///
/// The reason is asserted to be the note's own words rather than a second
/// sentence saying the same thing: two copies of this drift, and the drift is
/// invisible, because each channel stays individually correct while they
/// disagree about one read.
#[test]
fn the_graph_walk_says_it_is_not_and_says_why() {
    let held = points();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let (path, exact, rendered, notes) = reported(
        &mut session,
        "SELECT * FROM embeddings \
         ORDER BY vector::euclidean(vector, [0.0, 0.0]) LIMIT 5 APPROXIMATE;",
    );

    assert_eq!(path, AccessPath::Approximate);
    let Some(why) = exact.reason() else {
        panic!("the graph walk called itself exact");
    };
    assert_eq!(
        field(&rendered, "exact"),
        Some(&Value::Bool(false)),
        "the graph walk rendered {rendered:?}",
    );
    assert_eq!(field(&rendered, "inexact"), Some(&Value::from(why)));
    assert_eq!(notes, vec![Note::Approximate]);
    assert_eq!(
        why,
        Note::Approximate.message(),
        "the two channels stating this have drifted apart",
    );
}

/// The same read explained and run agree about exactness.
///
/// `Plan` exists so that `EXPLAIN` and an answer cannot describe one read in two
/// vocabularies, and exactness is the field where a disagreement would be worst:
/// a caller who checks the plan before running the statement would be told the
/// opposite of what the answer says.
#[test]
fn explaining_a_read_and_running_it_agree() {
    let held = points();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let read = "SELECT * FROM embeddings \
                ORDER BY vector::euclidean(vector, [0.0, 0.0]) LIMIT 5 APPROXIMATE";

    let (_, _, answered, _) = reported(&mut session, &format!("{read};"));
    let outcomes = session.run(&format!("EXPLAIN {read};")).unwrap();
    let Some(Outcome::Value(explained)) = outcomes.last() else {
        panic!("EXPLAIN answered with {:?}", outcomes.last());
    };

    for key in ["exact", "inexact"] {
        assert_eq!(
            field(explained, key),
            field(&answered, key),
            "EXPLAIN and the answer disagree about {key}",
        );
    }
    assert_eq!(field(explained, "exact"), Some(&Value::Bool(false)));
}

/// And the ordinary case of the same thing, so the test above is not passing on
/// a pair that happens to be wrong identically.
#[test]
fn explaining_an_exact_read_and_running_it_agree() {
    let held = texts(true);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let read = "SELECT * FROM notes WHERE city = 'Paris'";

    let (_, _, answered, _) = reported(&mut session, &format!("{read};"));
    let outcomes = session.run(&format!("EXPLAIN {read};")).unwrap();
    let Some(Outcome::Value(explained)) = outcomes.last() else {
        panic!("EXPLAIN answered with {:?}", outcomes.last());
    };

    assert_eq!(field(explained, "exact"), Some(&Value::Bool(true)));
    assert_eq!(field(&answered, "exact"), Some(&Value::Bool(true)));
    assert!(field(explained, "inexact").is_none());
}
