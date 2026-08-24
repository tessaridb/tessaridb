//! Asking a geo question from a script, against a store.
//!
//! The geometry crate's own tests establish that the predicates are right. These
//! establish something else, and it is the property wave 98 found missing the
//! last time: that the predicates are **reachable**. `is_well_formed` was
//! written, correct, tested, and called from nowhere, so invalid geometry was
//! accepted in silence for three waves. Existence and reachability are separate
//! properties of a piece of code, and a green kernel suite proves only the first.
//!
//! So nothing below calls a predicate. Every case writes records, runs a
//! statement, and reads the rows that came back.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::{Geometry, Number, Polygon, Position, Ring, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn schemaless(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE maps; USE NAMESPACE maps;\n\
             DEFINE DATABASE world; USE DATABASE world;\n\
             DEFINE TABLE places;",
        )
        .unwrap();
    session
}

fn at(longitude: f64, latitude: f64) -> Position {
    Position::new(longitude, latitude)
}

fn ring(corners: &[(f64, f64)]) -> Ring {
    let mut positions: Vec<Position> = corners
        .iter()
        .map(|&(longitude, latitude)| at(longitude, latitude))
        .collect();
    positions.push(positions[0]);
    Ring(positions)
}

fn square(low: f64, high: f64) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior: ring(&[(low, low), (high, low), (high, high), (low, high)]),
        interiors: Vec::new(),
    })
}

fn bound(name: &str, value: Value) -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert(name.to_owned(), value);
    parameters
}

/// Put a named place on the map.
fn place(session: &mut Session<'_>, id: u32, name: &str, shape: Geometry) {
    session
        .run_with(
            &format!("CREATE places:{id} = {{ name: '{name}', shape: $shape }};"),
            &bound("shape", Value::Geometry(shape)),
        )
        .unwrap();
}

/// The `name` of every record a statement answered with, sorted.
fn names(session: &mut Session<'_>, script: &str, parameters: &Parameters) -> Vec<String> {
    let outcomes = session.run_with(script, parameters).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    let mut found: Vec<String> = records
        .iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("a record is an object")
            };
            match fields.get("name") {
                Some(Value::String(name)) => name.clone(),
                other => panic!("every place has a name, this one has {other:?}"),
            }
        })
        .collect();
    found.sort();
    found
}

/// A map with one place of each interesting kind against the query square
/// `[2, 8] × [2, 8]`:
///
/// | place | shape | inside | on the edge | outside |
/// |---|---|---|---|---|
/// | `middle` | a position at (5, 5) | yes | | |
/// | `edge` | a position at (2, 5) | | yes | |
/// | `away` | a position at (20, 20) | | | yes |
/// | `small` | the square `[3, 7]²` | yes | | |
/// | `overlapping` | the square `[6, 12]²` | partly | | partly |
/// | `elsewhere` | the square `[30, 40]²` | | | yes |
/// | `nameless` | no `shape` field at all | | | |
fn a_map(session: &mut Session<'_>) {
    place(session, 1, "middle", Geometry::Point(at(5.0, 5.0)));
    place(session, 2, "edge", Geometry::Point(at(2.0, 5.0)));
    place(session, 3, "away", Geometry::Point(at(20.0, 20.0)));
    place(session, 4, "small", square(3.0, 7.0));
    place(session, 5, "overlapping", square(6.0, 12.0));
    place(session, 6, "elsewhere", square(30.0, 40.0));
    session
        .run("CREATE places:7 = { name: 'nameless' };")
        .unwrap();
}

fn query_square() -> Parameters {
    bound("area", Value::Geometry(square(2.0, 8.0)))
}

#[test]
fn intersects_answers_with_every_place_that_meets_the_query_square() {
    let store = store();
    let mut session = schemaless(&store);
    a_map(&mut session);

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM places WHERE geo::intersects(shape, $area);",
            &query_square(),
        ),
        // `edge` is in: a position on the boundary meets the square. `nameless`
        // is out because it has no shape, and it did not fail the read.
        vec!["edge", "middle", "overlapping", "small"]
    );
}

#[test]
fn disjoint_answers_with_exactly_the_places_intersects_left_out() {
    let store = store();
    let mut session = schemaless(&store);
    a_map(&mut session);

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM places WHERE geo::disjoint(shape, $area);",
            &query_square(),
        ),
        vec!["away", "elsewhere"]
    );
}

#[test]
fn contains_and_covers_disagree_about_a_place_on_the_boundary() {
    // The distinction the predicate layer is shaped around, asked in the
    // language rather than in the kernel. If these two ever return the same
    // rows, one of them has silently become the other.
    let store = store();
    let mut session = schemaless(&store);
    a_map(&mut session);

    let covered = names(
        &mut session,
        "SELECT * FROM places WHERE geo::covered_by(shape, $area);",
        &query_square(),
    );
    let contained = names(
        &mut session,
        "SELECT * FROM places WHERE geo::within(shape, $area);",
        &query_square(),
    );
    assert_eq!(covered, vec!["edge", "middle", "small"]);
    assert_eq!(contained, vec!["middle", "small"]);
}

#[test]
fn covers_reads_the_other_way_round_from_covered_by() {
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "big", square(0.0, 20.0));
    place(&mut session, 2, "small", square(1.0, 2.0));

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM places WHERE geo::covers(shape, $area);",
            &bound("area", Value::Geometry(square(3.0, 7.0))),
        ),
        vec!["big"]
    );
}

#[test]
fn equals_finds_the_shape_that_covers_the_same_positions_however_it_was_written() {
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "plain", square(0.0, 10.0));
    place(
        &mut session,
        2,
        "spelt-out",
        Geometry::Polygon(Polygon {
            // The same square with a redundant vertex halfway along its foot.
            exterior: ring(&[
                (0.0, 0.0),
                (5.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
            ]),
            interiors: Vec::new(),
        }),
    );
    place(&mut session, 3, "different", square(0.0, 9.0));

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM places WHERE geo::equals(shape, $area);",
            &bound("area", Value::Geometry(square(0.0, 10.0))),
        ),
        vec!["plain", "spelt-out"]
    );
}

#[test]
fn a_place_with_no_shape_narrows_the_answer_rather_than_failing_the_read() {
    // The absence rule, which is what lets a store of documents with differing
    // shapes have a function surface at all. Asserted directly, because the
    // fixtures above would pass whether the row was skipped or the read failed
    // only if the failure happened to be silent.
    let store = store();
    let mut session = schemaless(&store);
    session
        .run("CREATE places:1 = { name: 'nameless' };")
        .unwrap();

    assert!(
        names(
            &mut session,
            "SELECT * FROM places WHERE geo::intersects(shape, $area);",
            &query_square(),
        )
        .is_empty()
    );
}

#[test]
fn a_value_that_is_present_and_is_not_a_shape_is_a_mistake_and_says_so() {
    let store = store();
    let mut session = schemaless(&store);
    session
        .run("CREATE places:1 = { name: 'a place', shape: 'over there' };")
        .unwrap();

    let refusal = session
        .run_with(
            "SELECT * FROM places WHERE geo::intersects(shape, $area);",
            &query_square(),
        )
        .expect_err("a string is not a shape")
        .to_string();
    assert!(refusal.contains("geo::intersects"), "{refusal}");
    assert!(refusal.contains("geometry"), "{refusal}");
    assert!(refusal.contains("string"), "{refusal}");
}

#[test]
fn a_query_shape_off_the_sphere_is_refused_rather_than_wrapped_round_the_world() {
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "middle", Geometry::Point(at(5.0, 5.0)));

    let refusal = session
        .run_with(
            "SELECT * FROM places WHERE geo::intersects(shape, $area);",
            &bound("area", Value::Geometry(Geometry::Point(at(181.0, 0.0)))),
        )
        .expect_err("181 degrees east is not a place")
        .to_string();
    assert!(refusal.contains("longitude"), "{refusal}");
}

#[test]
fn a_geo_predicate_can_be_selected_as_a_field_and_not_only_filtered_on() {
    // A predicate that only worked in `WHERE` would be a predicate whose answer
    // a caller could never see, only act on.
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "middle", Geometry::Point(at(5.0, 5.0)));

    let outcomes = session
        .run_with(
            "SELECT geo::intersects(shape, $area) AS meets FROM places;",
            &query_square(),
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    let Value::Object(fields) = &records[0].1 else {
        panic!("a record is an object")
    };
    assert_eq!(
        fields.get("meets"),
        Some(&Value::Bool(true)),
        "the answer should come back as a value, got {fields:?}"
    );
}

// ----------------------------------------------------------- measurement

#[test]
fn distance_answers_in_metres_and_orders_a_bounded_read() {
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "near", Geometry::Point(at(0.0, 0.0)));
    place(&mut session, 2, "far", Geometry::Point(at(0.0, 5.0)));
    place(&mut session, 3, "middle", Geometry::Point(at(0.0, 1.0)));
    // A record with no shape at all, which is what the ordering has to survive.
    session
        .run("CREATE places:4 = { name: 'nowhere' };")
        .unwrap();

    let origin = bound("here", Value::Geometry(Geometry::Point(at(0.0, 0.0))));
    let outcomes = session
        .run_with(
            "SELECT * FROM places ORDER BY geo::distance(shape, $here) LIMIT 3;",
            &origin,
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    let ordered: Vec<String> = records
        .iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("a record is an object")
            };
            match fields.get("name") {
                Some(Value::String(name)) => name.clone(),
                other => panic!("expected a name, got {other:?}"),
            }
        })
        .collect();
    // `nowhere` is infinitely far rather than absent, so it sorts last rather
    // than first — which is the whole reason a distance answers for an absence.
    assert_eq!(ordered, vec!["near", "middle", "far"]);
}

#[test]
fn a_distance_between_shapes_larger_than_positions_is_refused_by_name() {
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "an area", square(0.0, 1.0));

    let refusal = session
        .run_with(
            "SELECT geo::distance(shape, $here) AS far FROM places;",
            &bound("here", Value::Geometry(Geometry::Point(at(5.0, 5.0)))),
        )
        .expect_err("a polygon is not a position")
        .to_string();
    assert!(refusal.contains("geo::distance"), "{refusal}");
    assert!(refusal.contains("position"), "{refusal}");
    assert!(refusal.contains("polygon"), "{refusal}");
}

#[test]
fn area_answers_in_square_metres_and_is_zero_without_an_interior() {
    let store = store();
    let mut session = schemaless(&store);
    place(&mut session, 1, "a square", square(0.0, 1.0));
    place(&mut session, 2, "a position", Geometry::Point(at(3.0, 3.0)));

    let outcomes = session
        .run("SELECT name, geo::area(shape) AS ground FROM places;")
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    let mut measured: Vec<(String, f64)> = records
        .iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("a record is an object")
            };
            let (Some(Value::String(name)), Some(Value::Number(Number::Float(ground)))) =
                (fields.get("name"), fields.get("ground"))
            else {
                panic!("expected a name and a float ground, got {fields:?}")
            };
            (name.clone(), *ground)
        })
        .collect();
    measured.sort_by(|one, other| one.0.cmp(&other.0));

    assert_eq!(measured[0].0, "a position");
    assert_eq!(measured[0].1, 0.0);

    // One degree square at the equator: a shade over twelve thousand square
    // kilometres. The exact value is the geometry crate's business and is
    // checked there against a closed form; what this asserts is that the answer
    // arrived in square metres rather than in degrees, which differ by fourteen
    // orders of magnitude and would be unmissable.
    assert_eq!(measured[1].0, "a square");
    assert!(
        measured[1].1 > 1.2e10 && measured[1].1 < 1.3e10,
        "a degree square should be about 1.23e10 m², got {}",
        measured[1].1
    );
}
