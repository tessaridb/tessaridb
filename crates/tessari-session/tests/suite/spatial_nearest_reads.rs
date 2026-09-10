//! `ORDER BY geo::distance(…) LIMIT k` stops being a scan, and answers the same
//! thing in the same order.
//!
//! # Why the comparison keeps the order
//!
//! Every other index test in this store may sort both answers before comparing
//! them, because an index changes cost and not content. This one may not. The
//! failure a nearest-first walk has is that it stops too early, and a walk that
//! stopped too early answers with **the wrong records** — real ones, near the
//! target, in a plausible order. Sorting the two answers before comparing them
//! would throw away the half of the evidence that shows it.
//!
//! # The refusals are the feature
//!
//! An ordering has nothing to re-test: the entry's position *is* the answer. So
//! more of this file is about the reads that must **not** be served this way
//! than about the ones that must, and each of those is a scan rather than an
//! approximation — an uncommitted write, an older snapshot, a hidden field, a
//! bound the index cannot fill, a sort the walk does not produce.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

const INDEX: &str = "DEFINE INDEX by_where ON stops FIELDS at SPATIAL;";

/// A table of positions, scattered so that no two are the same distance from any
/// of the targets below — a corpus already in the answer's order, or one full of
/// ties, could not tell a working walk from a broken one.
///
/// Every geometry is a **point**, because `geo::distance` takes positions. The
/// mixed case has its own test.
const STOPS: &str = "\
DEFINE NAMESPACE atlas; USE NAMESPACE atlas;
DEFINE DATABASE world; USE DATABASE world;
DEFINE COLLECTION stops;
CREATE stops:'gate' = { at: geometry { type: 'Point', coordinates: [2.35, 48.85] } };
CREATE stops:'bridge' = { at: geometry { type: 'Point', coordinates: [2.41, 48.79] } };
CREATE stops:'mill' = { at: geometry { type: 'Point', coordinates: [2.09, 48.93] } };
CREATE stops:'ford' = { at: geometry { type: 'Point', coordinates: [1.77, 49.21] } };
CREATE stops:'quay' = { at: geometry { type: 'Point', coordinates: [3.62, 47.48] } };
CREATE stops:'ridge' = { at: geometry { type: 'Point', coordinates: [-1.14, 51.02] } };
CREATE stops:'reach' = { at: geometry { type: 'Point', coordinates: [8.31, 44.67] } };
CREATE stops:'far' = { at: geometry { type: 'Point', coordinates: [-73.98, 40.75] } };
CREATE stops:'south' = { at: geometry { type: 'Point', coordinates: [18.42, -33.92] } };
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(STOPS).unwrap();
    session
}

/// The records a read answers with, **in the order it answered them**.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    let outcomes = session.run(read).unwrap();
    outcomes
        .last()
        .unwrap()
        .records()
        .unwrap_or_else(|| panic!("`{read}` did not answer with records"))
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

/// One field of the plan a read would take.
fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    match fields.get(field) {
        Some(Value::String(text)) => text.clone(),
        None => "absent".to_owned(),
        Some(other) => format!("{other:?}"),
    }
}

/// Ask `read` of a world with no spatial index and of one with it, require the
/// same answer **in the same order**, and hand back the plan the indexed read
/// took.
///
/// The plan comes back rather than being asserted here because "the answers
/// agree" and "the index was used" are two claims, and a test making only the
/// first would pass against a store that never touches the index at all.
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
        "`{read}` answered differently once the index existed"
    );
    plan(&mut with, read, "access")
}

/// A position written the way a statement writes one.
fn place(longitude: f64, latitude: f64) -> String {
    format!("geometry {{ type: 'Point', coordinates: [{longitude}, {latitude}] }}")
}

#[test]
fn a_bounded_order_by_distance_is_answered_from_the_index() {
    // The claim, at four bounds and from three places — one inside the cluster,
    // one beside it, one on another continent. Each is the whole assertion: the
    // scan's answer, in the scan's order, with the index reporting that it was
    // the one that produced it.
    for (longitude, latitude) in [(2.3, 48.8), (5.0, 46.0), (-70.0, 42.0)] {
        for bound in [1, 2, 4, 7] {
            let read = format!(
                "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT {bound};",
                place(longitude, latitude)
            );
            assert_eq!(
                same_either_way(&read),
                "ordered",
                "the index should have served `{read}`"
            );
        }
    }
}

#[test]
fn the_plan_says_which_kind_of_order_it_serves() {
    // A value order and a distance order both report `ordered`, and they are not
    // the same read: one takes entries already in the answer's order, the other
    // walks cells and ranks what it finds. The shape is what tells them apart,
    // and a plan that could not would make a regression from one to the other
    // invisible.
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    let read = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT 3;",
        place(2.3, 48.8)
    );
    assert_eq!(plan(&mut session, &read, "shape"), "nearest");
    assert_eq!(plan(&mut session, &read, "index"), "by_where");
}

#[test]
fn the_place_may_be_either_argument() {
    // `geo::distance(at, here)` and `geo::distance(here, at)` are one question
    // written two ways. A planner that recognised only the first would be
    // correct and silently unindexed for the second — a class of defect that
    // reports nothing at all, since the answer stays right and only the cost
    // moves.
    let here = place(2.3, 48.8);
    let field_first = format!("SELECT * FROM stops ORDER BY geo::distance(at, {here}) LIMIT 3;");
    let place_first = format!("SELECT * FROM stops ORDER BY geo::distance({here}, at) LIMIT 3;");
    assert_eq!(same_either_way(&field_first), "ordered");
    assert_eq!(same_either_way(&place_first), "ordered");
}

#[test]
fn a_start_is_added_to_what_the_walk_asks_for() {
    // `START` skips records the walk still has to find, so the bound it walks
    // for is the two together. A walk asking only for the limit would answer the
    // window from too small a pool and lose the records at its far edge.
    let read = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) START 4 LIMIT 3;",
        place(2.3, 48.8)
    );
    assert_eq!(same_either_way(&read), "ordered");
}

#[test]
fn a_bound_the_index_cannot_fill_falls_back_to_the_scan() {
    // A record with no geometry has no entry, and the value layer sorts an
    // absence *last* — so the answer beyond what the index holds needs records
    // the index does not have. The walk gives the read up rather than answering
    // short, which is the same rule an ordered walk follows.
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    session
        .run("CREATE stops:'nowhere' = { name: 'no place' };")
        .unwrap();

    let inside = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT 9;",
        place(2.3, 48.8)
    );
    let beyond = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT 10;",
        place(2.3, 48.8)
    );
    assert_eq!(plan(&mut session, &inside, "access"), "ordered");
    assert_eq!(plan(&mut session, &beyond, "access"), "ordered");
    // The plan cannot ask whether the walk will fill the bound — that is the
    // read itself — so the difference shows in the answer rather than the plan.
    let found = ids(&mut session, &beyond);
    assert_eq!(found.len(), 10);
    assert_eq!(found.last(), Some(&RecordId::from("nowhere")));
}

#[test]
fn a_record_holding_a_shape_answers_the_same_mistake_either_way() {
    // `geo::distance` takes positions, so a record holding an area is an error
    // in the statement. The walk must not answer the other records in distance
    // order and quietly leave the mistake unreported, which would be an index
    // changing what a read *answers* rather than what it costs.
    let read = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT 3;",
        place(2.3, 48.8)
    );
    let area = "CREATE stops:'field' = { at: geometry { type: 'LineString', \
                coordinates: [[2.30, 48.80], [2.31, 48.81]] } };";

    let bare = store();
    let mut without = ready(&bare);
    without.run(area).unwrap();
    let scanned = without.run(&read);

    let indexed = store();
    let mut with = ready(&indexed);
    with.run(INDEX).unwrap();
    with.run(area).unwrap();
    let served = with.run(&read);

    assert!(
        scanned.is_err(),
        "a shape is not a position and must be refused"
    );
    assert_eq!(served.is_err(), scanned.is_err());
    assert_eq!(
        format!("{:?}", served.err()),
        format!("{:?}", scanned.err()),
        "the index answered a different mistake from the scan"
    );
}

#[test]
fn an_ordered_index_on_the_field_does_not_capture_a_distance_order() {
    // The positive-guard case. `index_serving_place` asks whether the index is
    // spatial, and deleting that question breaks nothing unless a store exists
    // where some *other* index sits on the same field: the walk would then be
    // handed a keyspace laid out by value, find nothing in it, and answer with
    // no rows and no error.
    let indexed = store();
    let mut session = ready(&indexed);
    session
        .run("DEFINE INDEX by_value ON stops FIELDS at;")
        .unwrap();
    let read = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT 3;",
        place(2.3, 48.8)
    );
    assert_eq!(plan(&mut session, &read, "access"), "scan");
    assert_eq!(ids(&mut session, &read).len(), 3);
}

#[test]
fn every_shape_the_walk_does_not_produce_stays_a_scan() {
    // Each of these is a way the order a walk yields differs from the order the
    // statement asked for, and each falls to the scan rather than to an
    // approximation of it.
    let here = place(2.3, 48.8);
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    for (why, read) in [
        (
            "descending — a distance orders ascending",
            format!("SELECT * FROM stops ORDER BY geo::distance(at, {here}) DESC LIMIT 3;"),
        ),
        (
            "a second key orders records the walk never ranked",
            format!("SELECT * FROM stops ORDER BY geo::distance(at, {here}), id LIMIT 3;"),
        ),
        (
            "no bound, so there is nothing to stop at",
            format!("SELECT * FROM stops ORDER BY geo::distance(at, {here});"),
        ),
        (
            "APPROXIMATE is the vector shape and is recognised on its own",
            format!("SELECT * FROM stops ORDER BY geo::distance(at, {here}) LIMIT 3 APPROXIMATE;"),
        ),
        (
            "GROUP BY folds the records the order would have chosen between",
            format!(
                "SELECT at, count(*) AS n FROM stops GROUP BY at ORDER BY geo::distance(at, {here}) LIMIT 3;"
            ),
        ),
        (
            "a distance between two fields is not a distance from a place",
            "SELECT * FROM stops ORDER BY geo::distance(at, at) LIMIT 3;".to_owned(),
        ),
    ] {
        assert_eq!(
            plan(&mut session, &read, "access"),
            "scan",
            "{why}: `{read}` must not be served nearest-first"
        );
    }
}

#[test]
fn a_projection_the_order_can_read_past_keeps_the_walk() {
    // The blanket refusal this replaces (Q-390) was written for the reason
    // `ordered` still refuses every projection: a sort key reads the **answer's**
    // names, so a projection may put something else under the name the key
    // reads. A distance differs in the one way that decides it — the ordering
    // stage lays the source record beneath the projection precisely so a key can
    // reach a field the projection dropped (`consume::reach_past`), and the case
    // that overlay was built for (Q-143) is this statement, written out.
    //
    // Two shapes, because they fail differently if the narrowing is wrong: one
    // that drops the geometry entirely, and one that writes it out under its own
    // name — which is a shadow of the field by itself and changes nothing.
    let here = place(2.3, 48.8);
    for read in [
        format!("SELECT id FROM stops ORDER BY geo::distance(at, {here}) LIMIT 3;"),
        format!("SELECT at FROM stops ORDER BY geo::distance(at, {here}) LIMIT 3;"),
    ] {
        assert_eq!(
            same_either_way(&read),
            "ordered",
            "`{read}` answers what the scan answers and should keep its bound"
        );
    }
}

#[test]
fn a_projection_answering_under_the_geometry_name_falls_back_to_the_scan() {
    // The one shape the overlay cannot undo: a name the projection **offers**.
    // Here the answer carries an `at` the index does not hold, so the order the
    // statement asks for is not the order the entries are in — and the walk has
    // nothing to re-test the difference against.
    //
    // The shadow is a constant because the fixture holds one geometry per
    // record; what is under test is which `at` the key reads, not how far apart
    // the two are.
    let here = place(2.3, 48.8);
    let elsewhere = place(-73.98, 40.75);
    let read =
        format!("SELECT {elsewhere} AS at FROM stops ORDER BY geo::distance(at, {here}) LIMIT 3;");
    assert_eq!(
        same_either_way(&read),
        "scan",
        "a projection answering under the searched field's own name must not be walked"
    );
}

#[test]
fn a_write_in_this_transaction_sends_the_read_back_to_the_scan() {
    // Entries are derived at commit, so a record written and not committed has
    // none and the walk cannot place it. Answering from the index would leave it
    // out of an order it belongs in — and unlike a filtered read there is
    // nothing to re-test it against afterwards.
    let indexed = store();
    let mut session = ready(&indexed);
    session.run(INDEX).unwrap();
    let read = format!(
        "SELECT * FROM stops ORDER BY geo::distance(at, {}) LIMIT 3;",
        place(2.3, 48.8)
    );
    assert_eq!(plan(&mut session, &read, "access"), "ordered");

    let outcomes = session
        .run(&format!(
            "BEGIN; CREATE stops:'new' = {{ at: {} }}; EXPLAIN {read} COMMIT;",
            place(2.31, 48.81)
        ))
        .unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes
        .iter()
        .find(|outcome| matches!(outcome, Outcome::Value(Value::Object(_))))
    else {
        panic!("the plan inside the transaction is missing: {outcomes:?}");
    };
    assert_eq!(
        fields.get("access"),
        Some(&Value::from("scan")),
        "an uncommitted write has no entry, so the order cannot come from the index"
    );
}
