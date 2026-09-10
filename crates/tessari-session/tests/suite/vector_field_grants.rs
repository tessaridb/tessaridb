//! What the vector walk does for a caller whose grant hides the vectors.
//!
//! # The third walk
//!
//! Q-391 asked whether an index-served read can reach a field the caller's grant
//! excludes. W186 answered it for the search and ordered walks and named the
//! vector walk as the remaining third rather than folding it into "the bounded
//! walks are covered" — a vector fixture is a second setup, and claiming
//! coverage the run does not support is the failure that question exists to
//! record.
//!
//! # What is disclosed is the set, not the order
//!
//! A field permission removes the field *before* anything reads the record, so a
//! caller without it sorts by `none`. `index_serving_place` refuses an index for
//! that reason in its own words — an order taken from the index would sort by
//! the values themselves, disclosing what the projection hides one comparison at
//! a time — and `index_serving_score` and `index_serving_order` refuse for it
//! too.
//!
//! On the vector walk the leak turns out to be **larger than an order**, and the
//! first run of this file is what said so. The graph chooses the five records
//! nearest the query and hands them on; the `none` sort key then re-sorts those
//! five among themselves, so the answer arrives in tidy identity order. It looks
//! like an ordinary unordered result and it is a **membership** answer: these
//! five records are the ones closest to a vector this caller may not read. An
//! order can be inferred from repeated reads; a set is handed over in one.
//!
//! That is also why the third case below compares record *sets*. Comparing the
//! answers as ordered lists passes today — the two differ in order alone — which
//! is precisely the shape of assertion this project keeps catching after the
//! fact.
//!
//! # What is asserted, and what deliberately is not
//!
//! **The property, not the path.** For a caller who cannot read the vectors the
//! sort key is `none` on every record, so the exact read and the approximate one
//! are answering the same unordered question. The assertion is therefore that
//! they agree — which names no mechanism. A store that declines to serve the
//! graph satisfies it; so would one that serves it and re-orders afterwards.
//! What it refuses is the store handing that caller true distance order.
//!
//! Asserting instead that the narrow caller's plan reports a scan would pin one
//! of several sound mechanisms, and W186 recorded the cost of that mistake: the
//! assertion failed against a store that was behaving correctly.
//!
//! # Why the fixture puts the nearest record last
//!
//! The query is the *far end* of the line, so the vector order is the exact
//! reverse of identity order. A fixture whose nearest record is also its first
//! would let both paths answer identically and prove nothing — the vacuous shape
//! this suite has hit more than once. Here the two orders cannot coincide, so a
//! walk that ran when it should not have is visible in the first record alone.
//!
//! # The control
//!
//! The wide caller's read must report `AccessPath::Approximate` and come back
//! nearest-first. Without it a store that served nobody would pass every case
//! here, and the differential would say nothing about grants.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

const PASSWORD: &str = "correct horse battery";

/// How many points the line carries.
///
/// Enough that the graph has somewhere to walk and the answer is a small slice
/// of the table rather than most of it.
const POINTS: u32 = 40;

/// How many records each read asks for.
const WANTED: usize = 5;

/// The far end of the line, so the nearest record is the last one written.
const NEAR: &str = "SELECT * FROM points \
                    ORDER BY vector::euclidean(embedding, [39.0, 0.0]) LIMIT 5";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Forty points on a line, a vector index over them, and two editors — one who
/// holds the field the index is built over and one who does not.
fn ready(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE atlas; USE DATABASE atlas;\n\
             DEFINE COLLECTION points;\n\
             DEFINE FIELD embedding ON points TYPE vector<2> REQUIRED;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 0..POINTS {
        // Written in identity order along the line, so identity order and
        // distance order from the far end are exact reverses of one another.
        script.push_str(&format!(
            "CREATE points:{n} = {{ name: 'point {n}', embedding: [{n}.0, 0.0] }};\n"
        ));
    }
    session.run(&script).unwrap();
    session
        .run(
            "DEFINE INDEX by_embedding ON points FIELDS embedding VECTOR euclidean;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "DEFINE USER wide ON prod.atlas ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER narrow ON prod.atlas ROLE editor PASSWORD 'correct horse battery';\n\
         USE NAMESPACE prod; USE DATABASE atlas;\n\
         GRANT read ON points FIELDS name, embedding TO wide;\n\
         GRANT read ON points FIELDS name TO narrow;",
    )
    .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE atlas;")
        .unwrap();
    session
}

/// The records a read answered with, in the order it answered them, and the path
/// that served it.
fn answered(session: &mut Session<'_>, script: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        plan.access,
    )
}

#[test]
fn the_wide_caller_is_served_by_the_graph_nearest_first() {
    // The control. Every other case here is a differential against this one, and
    // without it a store that served nobody would pass them all while saying
    // nothing about grants.
    let held = store();
    ready(&held);
    let mut wide = signed_in(&held, "wide");

    let (found, path) = answered(&mut wide, &format!("{NEAR} APPROXIMATE;"));
    assert_eq!(
        path,
        AccessPath::Approximate,
        "the graph did not serve {path:?}"
    );
    assert_eq!(found.len(), WANTED, "{found:?}");
    // The far end of the line: the last record written is the nearest one.
    assert_eq!(found[0], RecordId::from(39_i64), "{found:?}");
    assert_eq!(found[4], RecordId::from(35_i64), "{found:?}");
}

#[test]
fn the_narrow_caller_is_not_ordered_by_the_vectors_it_cannot_read() {
    // The case Q-391 is about. For this caller `embedding` is removed before
    // anything reads the record, so the sort key is `none` on every record and
    // the two reads are asking the same unordered question. They must therefore
    // answer the same thing — which is a claim about what the caller sees and
    // not about which path the store took to arrange it.
    let held = store();
    ready(&held);
    let mut narrow = signed_in(&held, "narrow");

    let (exact, _) = answered(&mut narrow, &format!("{NEAR};"));
    let (approximate, _) = answered(&mut narrow, &format!("{NEAR} APPROXIMATE;"));

    assert_eq!(
        approximate, exact,
        "a caller who cannot read the vectors was answered from the graph: \
         approximate={approximate:?} exact={exact:?}"
    );
}

#[test]
fn the_narrow_caller_is_not_handed_the_records_the_graph_chose() {
    // The membership half, and it needs its own case because the one above would
    // also hold if the store served the graph to *both* of that caller's reads —
    // a way of failing that looks like agreement.
    //
    // The comparison is between SETS on purpose. As ordered lists the two
    // answers differ whatever happens, because the `none` sort key re-sorts
    // whatever the graph returned back into identity order; so a list comparison
    // here passes against a store that is disclosing the whole set, which is the
    // assertion this file was drafted with and the run corrected.
    let held = store();
    ready(&held);
    let mut wide = signed_in(&held, "wide");
    let mut narrow = signed_in(&held, "narrow");

    let (served, path) = answered(&mut wide, &format!("{NEAR} APPROXIMATE;"));
    assert_eq!(
        path,
        AccessPath::Approximate,
        "the control did not hold: {path:?}"
    );
    let (hidden, _) = answered(&mut narrow, &format!("{NEAR} APPROXIMATE;"));

    let chosen: BTreeSet<RecordId> = served.into_iter().collect();
    let handed: BTreeSet<RecordId> = hidden.into_iter().collect();
    assert_ne!(
        handed, chosen,
        "the caller who cannot read the vectors was handed the records the graph \
         chose by them"
    );
}
