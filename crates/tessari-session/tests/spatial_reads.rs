//! A geometry question stops being a scan, and answers the same thing.
//!
//! # The one property, and why it is the only one worth asserting from here
//!
//! An index changes what a read **costs** and never what it **answers**. Every
//! test here is a form of that sentence: run the question with no index, run it
//! again with one, and require the two answers to be identical. Nothing is
//! compared against a list of record names written down by hand, because a
//! hand-written list has to be re-derived by whoever adds a case and is wrong in
//! exactly the direction the reader is least likely to check.
//!
//! Which candidates the filter produced, and how many of them the exact
//! predicate then threw away, is asserted where it can be seen — the storage
//! test `spatial_region.rs`, against an oracle. From up here a surplus candidate
//! is invisible: the answer is right and only the cost is wrong.
//!
//! # The scales are the generator, not decoration
//!
//! The boxes run from a hundred metres to half the planet, and the records from
//! a point to a continent. Both directions have caught something in this
//! project: a covering test that only drew world-scale boxes, and an ordering
//! test that only placed entries at the finest level. Each passed with the exact
//! defect it existed to catch already present.
//!
//! The record scale is the sharper of the two. A record **larger** than the
//! query sits at a coarser cell, reachable only by the lookups above the query's
//! own cells — so a reader missing that half answers small questions perfectly
//! and loses precisely the large rows.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};

const INDEX: &str = "DEFINE INDEX by_where ON places FIELDS at SPATIAL;";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A table of places at every scale from a point to a continent.
///
/// Written once and shared by every case, so a question is asked of the same
/// world each time and a difference between two answers is a difference between
/// two questions.
const PLACES: &str = "\
DEFINE NAMESPACE atlas; USE NAMESPACE atlas;
DEFINE DATABASE world; USE DATABASE world;
DEFINE COLLECTION places;
CREATE places:'spot' = { name: 'a point', at: geometry { type: 'Point', coordinates: [2.35, 48.85] } };
CREATE places:'corner' = { name: 'a block', at: geometry { type: 'LineString', coordinates: [[2.35, 48.85], [2.351, 48.851]] } };
CREATE places:'quarter' = { name: 'a district', at: geometry { type: 'LineString', coordinates: [[2.3, 48.8], [2.4, 48.9]] } };
CREATE places:'town' = { name: 'a city', at: geometry { type: 'Polygon', coordinates: [[[2.2, 48.8], [2.5, 48.8], [2.5, 49.0], [2.2, 49.0], [2.2, 48.8]]] } };
CREATE places:'province' = { name: 'a region', at: geometry { type: 'Polygon', coordinates: [[[0.0, 47.0], [5.0, 47.0], [5.0, 50.0], [0.0, 50.0], [0.0, 47.0]]] } };
CREATE places:'landmass' = { name: 'a continent', at: geometry { type: 'Polygon', coordinates: [[[-20.0, 30.0], [40.0, 30.0], [40.0, 70.0], [-20.0, 70.0], [-20.0, 30.0]]] } };
CREATE places:'away' = { name: 'elsewhere', at: geometry { type: 'Point', coordinates: [-73.98, 40.75] } };
CREATE places:'blank' = { name: 'no geometry at all' };
";

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(PLACES).unwrap();
    session
}

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    let outcomes = session.run(read).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap_or_else(|| panic!("`{read}` did not answer with records"))
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

/// One field of the plan a read would take.
fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    match fields.get(field) {
        Some(Value::String(text)) => text.clone(),
        // Absent and present-but-empty are different answers, and a plan that
        // omitted a field would otherwise read as one that reported nothing.
        None => "absent".to_owned(),
        Some(other) => format!("{other:?}"),
    }
}

/// Ask `read` of a world with no spatial index and of one with it, and require
/// the same answer.
///
/// Returns the plan the indexed read took, so a caller can say whether it
/// expected the index to be chosen — the two questions are separate and a test
/// asserting only the first would pass against a store that never uses the index
/// at all.
fn same_either_way(read: &str) -> String {
    let bare = store();
    let mut without = ready(&bare);
    let scanned = ids(&mut without, read);
    assert_eq!(
        plan(&mut without, read, "access"),
        "scan",
        "with no index declared, `{read}` has to be a scan"
    );

    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();
    let served = ids(&mut with, read);

    assert_eq!(
        served, scanned,
        "`{read}` answered {served:?} through the index and {scanned:?} through the \
         scan — an index may change what a read costs, never what it says"
    );
    plan(&mut with, read, "access")
}

/// A box, as a query polygon, from two corners in degrees.
fn window(west: f64, south: f64, east: f64, north: f64) -> String {
    format!(
        "geometry {{ type: 'Polygon', coordinates: [[[{west}, {south}], [{east}, {south}], \
         [{east}, {north}], [{west}, {north}], [{west}, {south}]]] }}"
    )
}

/// Query windows from a city block to half the planet, all over the same place,
/// so each one is a strictly larger question than the last.
fn windows() -> Vec<String> {
    vec![
        window(2.3495, 48.8495, 2.3505, 48.8505),
        window(2.34, 48.84, 2.36, 48.86),
        window(2.0, 48.5, 2.7, 49.2),
        window(-5.0, 42.0, 10.0, 55.0),
        window(-90.0, 0.0, 90.0, 80.0),
        // One that holds nothing at all: a filter is as wrong when it invents a
        // row as when it loses one, and an empty answer is where an invented one
        // stands out.
        window(150.0, -40.0, 155.0, -35.0),
    ]
}

#[test]
fn every_servable_predicate_answers_what_the_scan_answers() {
    let mut served = 0_usize;
    for predicate in [
        "geo::intersects",
        "geo::within",
        "geo::covered_by",
        "geo::contains",
        "geo::covers",
        "geo::equals",
        "geo::touches",
    ] {
        for query in windows() {
            let read = format!("SELECT * FROM places WHERE {predicate}(at, {query});");
            let access = same_either_way(&read);
            assert_eq!(
                access, "index",
                "`{predicate}` over a box should be served by the spatial index"
            );
            served = served.saturating_add(1);
        }
    }
    assert!(served >= 42, "every predicate should meet every window");
}

#[test]
fn a_record_larger_than_the_query_is_still_in_the_answer() {
    // The ancestor lookups, end to end. The window is a city block; the
    // continent's cells are coarse and lie *above* it, where no forward scan
    // from the block's own cells will ever pass them.
    let read = format!(
        "SELECT * FROM places WHERE geo::intersects(at, {});",
        window(2.3495, 48.8495, 2.3505, 48.8505)
    );
    let access = same_either_way(&read);
    assert_eq!(access, "index");

    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    let found = ids(&mut session, &read);
    for expected in ["landmass", "province", "town", "quarter", "corner", "spot"] {
        assert!(
            found.contains(&RecordId::from(expected)),
            "a block-sized window should still find `{expected}`; found {found:?}"
        );
    }
    assert!(!found.contains(&RecordId::from("away")));
}

#[test]
fn the_field_may_be_either_argument() {
    // `geo::contains(at, Q)` and `geo::contains(Q, at)` are different questions
    // and both are ordinary to write. A planner understanding only the first
    // would leave half the natural phrasings on the scan — slow, not wrong,
    // which is why nothing would ever report it.
    let query = window(2.0, 48.5, 2.7, 49.2);
    for (one, other) in [
        (
            format!("geo::contains(at, {query})"),
            format!("geo::within({query}, at)"),
        ),
        (
            format!("geo::within(at, {query})"),
            format!("geo::contains({query}, at)"),
        ),
        (
            format!("geo::intersects(at, {query})"),
            format!("geo::intersects({query}, at)"),
        ),
    ] {
        let first = format!("SELECT * FROM places WHERE {one};");
        let second = format!("SELECT * FROM places WHERE {other};");
        assert_eq!(same_either_way(&first), "index", "`{one}` should be served");
        assert_eq!(
            same_either_way(&second),
            "index",
            "`{other}` should be served"
        );

        let indexed = store();
        let mut session = ready(&indexed);
        session.run(INDEX).unwrap();
        assert_eq!(
            ids(&mut session, &first),
            ids(&mut session, &second),
            "`{one}` and `{other}` ask the same question and must answer alike"
        );
    }
}

#[test]
fn disjoint_stays_a_scan() {
    // It is the complement of a region, and a complement has no set of cells:
    // every record whose box misses the window is disjoint, and so is every
    // record whose box meets it but whose shape does not. Serving it from the
    // cells would answer with a fraction of the true set and raise nothing.
    let read = format!(
        "SELECT * FROM places WHERE geo::disjoint(at, {});",
        window(2.0, 48.5, 2.7, 49.2)
    );
    assert_eq!(
        same_either_way(&read),
        "scan",
        "`geo::disjoint` has no sound box filter and must stay exact"
    );
    let bare = store();
    let mut session = ready(&bare);
    assert!(
        !ids(&mut session, &read).is_empty(),
        "the case proves nothing unless the scan finds something"
    );
}

#[test]
fn a_negated_or_alternative_geometry_test_stays_a_scan() {
    // The same two doors every other index shape is closed at. Under `NOT` an
    // index that finds the matching records finds exactly the wrong set; under
    // `OR` neither side alone narrows, because a record satisfying the other
    // half would be lost.
    let query = window(2.0, 48.5, 2.7, 49.2);
    for read in [
        format!("SELECT * FROM places WHERE NOT geo::intersects(at, {query});"),
        format!("SELECT * FROM places WHERE geo::intersects(at, {query}) OR name = 'elsewhere';"),
    ] {
        assert_eq!(
            same_either_way(&read),
            "scan",
            "`{read}` must not be served from one side of it"
        );
    }
}

#[test]
fn a_geometry_test_against_another_field_stays_a_scan() {
    // Neither argument is a constant, so there is no query box to cover. The
    // read is a scan and is exact, which is the honest answer rather than a
    // covering of whatever the first record happened to hold.
    let read = "SELECT * FROM places WHERE geo::intersects(at, at);";
    assert_eq!(same_either_way(read), "scan");
}

#[test]
fn the_plan_names_the_shape_and_how_much_of_the_key_space_it_touches() {
    // `EXPLAIN` is what somebody diagnoses a slow query from, and "index" alone
    // does not distinguish a point lookup from a covering of half the planet.
    // The cell count is the one cost of a region read a plan can know without
    // running it — the candidate-to-result ratio needs the read, and is not
    // invented here.
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();

    let small = format!(
        "SELECT * FROM places WHERE geo::intersects(at, {});",
        window(2.3495, 48.8495, 2.3505, 48.8505)
    );
    assert_eq!(plan(&mut session, &small, "access"), "index");
    assert_eq!(plan(&mut session, &small, "shape"), "region");
    assert_eq!(plan(&mut session, &small, "index"), "by_where");
    let cells = plan(&mut session, &small, "cells");
    assert_ne!(
        cells, "absent",
        "a region plan should say how many cells it reads"
    );

    // A read no spatial index serves carries no cell count, rather than a zero
    // that would read as "it looked and found none".
    let elsewhere = "SELECT * FROM places WHERE name = 'a city';";
    assert_eq!(plan(&mut session, elsewhere, "cells"), "absent");
}

#[test]
fn an_ordered_index_still_wins_the_question_it_is_for() {
    // The region shape sorts last, so the test that matters most is that adding
    // it moved nothing else: an equality on an ordered index must still be
    // served by that index and not lose a tie to a spatial one on another field.
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    session
        .run("DEFINE INDEX by_name ON places FIELDS name;")
        .unwrap();

    let read = "SELECT * FROM places WHERE name = 'a city';";
    assert_eq!(plan(&mut session, read, "access"), "index");
    assert_eq!(plan(&mut session, read, "index"), "by_name");
    assert_eq!(plan(&mut session, read, "shape"), "equality");
    assert_eq!(ids(&mut session, read), vec![RecordId::from("town")]);
}

#[test]
fn an_ordered_index_on_the_field_does_not_capture_a_geometry_test() {
    // The leak in the other direction, and the falsification pass is why it
    // exists. `spatial()` asks `index.spatial` positively — but nothing tested
    // that it does, because every other case here declares a spatial index and
    // the guard is only consulted for a `geo::` conjunct. Deleting the check
    // entirely broke nothing in the suite.
    //
    // What it would break: an ordinary index on the same field would be offered
    // for a region read, the read would look for cells in a keyspace keyed by
    // values, find none, and answer with **zero rows and no error** — which is
    // exactly the failure `c507363` fixed coming back through the door this wave
    // opened.
    let read = format!(
        "SELECT * FROM places WHERE geo::intersects(at, {});",
        window(2.0, 48.5, 2.7, 49.2)
    );

    let bare = store();
    let mut session = ready(&bare);
    let scanned = ids(&mut session, &read);
    assert!(
        !scanned.is_empty(),
        "the case proves nothing on an empty answer"
    );

    // An ordered index on the very field the geometry test names, and no
    // spatial one anywhere.
    session
        .run("DEFINE INDEX by_at ON places FIELDS at;")
        .unwrap();
    assert_eq!(
        plan(&mut session, &read, "access"),
        "scan",
        "an ordered index cannot answer about cells and must not be offered"
    );
    assert_eq!(ids(&mut session, &read), scanned);
}

#[test]
fn a_region_and_an_equality_in_one_condition_choose_the_equality() {
    // Both are offered and the ranking decides. An equality on an ordered index
    // is a lookup by value; a region is a set of candidates to refine. Region
    // sorts last deliberately, and this is where that ordering is visible.
    let read = format!(
        "SELECT * FROM places WHERE name = 'a city' AND geo::intersects(at, {});",
        window(2.0, 48.5, 2.7, 49.2)
    );
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    session
        .run("DEFINE INDEX by_name ON places FIELDS name;")
        .unwrap();
    assert_eq!(plan(&mut session, &read, "shape"), "equality");
    assert_eq!(ids(&mut session, &read), vec![RecordId::from("town")]);
}

#[test]
fn a_record_with_no_geometry_is_never_in_a_geometric_answer() {
    // It has no entry, so the index cannot offer it — and the condition would
    // refuse it anyway. Both have to be true: an index that offered it would be
    // relying on the condition to clean up after it, and a condition that
    // accepted it would be answering about a shape that is not there.
    let read = format!(
        "SELECT * FROM places WHERE geo::intersects(at, {});",
        window(-90.0, 0.0, 90.0, 80.0)
    );
    let access = same_either_way(&read);
    assert_eq!(access, "index");
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    assert!(!ids(&mut session, &read).contains(&RecordId::from("blank")));
}
