//! Writing a shape in a script.
//!
//! # Why a literal exists at all when a parameter already works
//!
//! A bound parameter is a complete path for a **client**, and it is what the SDK
//! uses. It is no use to two other readers: a person at a prompt, and the
//! conformance corpus — which is the executable definition of the language and
//! can only run scripts. Until this landed, the geometry half of the language
//! had no definition there at all.
//!
//! # What the marker is, and what it is not
//!
//! `geometry` is a **contextual** word, not a reserved one. Reserving it would
//! take a usable table and field name away from data that already exists, and
//! the brace after it is enough to tell the two apart — a table name is never
//! followed by an object. The cases below hold both halves of that.

#![allow(clippy::panic)]

use tessari_ql::{Error, ExprKind, StatementKind, parse};
use tessari_types::{Geometry, Position, Value};

/// The value the first statement sets, when it is a `SET`.
fn value_of(script: &str) -> Value {
    let parsed = parse(script).unwrap_or_else(|error| panic!("{script:?} should parse: {error}"));
    let Some(StatementKind::Set { value, .. }) =
        parsed.statements.first().map(|statement| &statement.kind)
    else {
        panic!("{script:?} should be a set")
    };
    match &value.kind {
        ExprKind::Literal(held) => held.clone(),
        other => panic!("a shape literal should be a literal, got {other:?}"),
    }
}

fn shape_of(script: &str) -> Geometry {
    match value_of(script) {
        Value::Geometry(shape) => shape,
        other => panic!("expected a shape, got {other:?}"),
    }
}

fn refusal(script: &str) -> Error {
    parse(script).expect_err("this should be refused")
}

#[test]
fn a_position_is_written_longitude_first() {
    assert_eq!(
        shape_of("SET here:0 = geometry { type: 'Point', coordinates: [2.35, 48.85] };"),
        Geometry::Point(Position::new(2.35, 48.85))
    );
}

#[test]
fn a_coordinate_west_or_south_of_zero_is_written_with_a_minus() {
    // A negative number reaches the parser as a negation of a positive one, so
    // this is the case a literal reader that only accepted number tokens would
    // fail on — and half the planet is west of Greenwich.
    assert_eq!(
        shape_of("SET here:0 = geometry { type: 'Point', coordinates: [-73.99, -33.86] };"),
        Geometry::Point(Position::new(-73.99, -33.86))
    );
}

#[test]
fn a_whole_number_coordinate_needs_no_decimal_point() {
    assert_eq!(
        shape_of("SET here:0 = geometry { type: 'Point', coordinates: [1, -2] };"),
        Geometry::Point(Position::new(1.0, -2.0))
    );
}

#[test]
fn every_one_of_the_seven_shapes_can_be_written() {
    let scripts = [
        "geometry { type: 'Point', coordinates: [0, 0] }",
        "geometry { type: 'LineString', coordinates: [[0, 0], [1, 1]] }",
        "geometry { type: 'Polygon', coordinates: [[[0, 0], [1, 0], [1, 1], [0, 0]]] }",
        "geometry { type: 'MultiPoint', coordinates: [[0, 0], [1, 1]] }",
        "geometry { type: 'MultiLineString', coordinates: [[[0, 0], [1, 1]]] }",
        "geometry { type: 'MultiPolygon', coordinates: [[[[0, 0], [1, 0], [1, 1], [0, 0]]]] }",
        "geometry { type: 'GeometryCollection', geometries: [{ type: 'Point', coordinates: [0, 0] }] }",
    ];
    let names: Vec<&'static str> = scripts
        .iter()
        .map(|written| shape_of(&format!("SET here:0 = {written};")).kind_name())
        .collect();
    assert_eq!(
        names,
        vec![
            "point",
            "line",
            "polygon",
            "multipoint",
            "multiline",
            "multipolygon",
            "collection"
        ]
    );
}

#[test]
fn a_polygon_keeps_its_holes_apart_from_its_shell() {
    let Geometry::Polygon(area) = shape_of(
        "SET here:0 = geometry { type: 'Polygon', coordinates: [\
         [[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]],\
         [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]]] };",
    ) else {
        panic!("a polygon stays a polygon")
    };
    assert_eq!(area.exterior.0.len(), 5);
    assert_eq!(area.interiors.len(), 1);
}

#[test]
fn the_marker_is_contextual_and_the_word_stays_a_usable_name() {
    // Without the brace, `geometry` is a table like any other word — which is
    // what reserving the word would have taken away.
    let parsed = parse("SELECT * FROM geometry;").expect("a table may be called that");
    assert_eq!(parsed.statements.len(), 1);

    // And it is a usable field name, both written and read.
    parse("CREATE shapes:1 = { geometry: 'not a shape' };").expect("a field may be called that");
    parse("SELECT * FROM shapes WHERE geometry = 'not a shape';").expect("and read back");
}

#[test]
fn a_shape_that_has_to_be_computed_is_refused_as_a_literal() {
    // A literal is read at parse time, so a parameter inside one would have to be
    // evaluated — and a shape that could differ per record is not a literal. Such
    // a shape is written as a bound parameter instead.
    let error = refusal("SET here:0 = geometry { type: 'Point', coordinates: [$east, 0] };");
    assert!(
        matches!(error, Error::ComputedGeometry { .. }),
        "expected a computed-shape refusal, got {error}"
    );
}

#[test]
fn a_malformed_shape_is_refused_in_the_readers_own_words() {
    for (script, expected) in [
        (
            "SET here:0 = geometry { type: 'Circle', coordinates: [0, 0] };",
            "not a shape name",
        ),
        (
            "SET here:0 = geometry { type: 'Point', coordinates: [0, 0, 5] };",
            "no altitude",
        ),
        (
            "SET here:0 = geometry { type: 'Polygon' };",
            "needs a `coordinates`",
        ),
        (
            "SET here:0 = geometry { type: 'Point', coordinates: ['east', 0] };",
            "a coordinate is an integer or a float",
        ),
    ] {
        let message = refusal(script).to_string();
        assert!(
            message.contains(expected),
            "{script:?} should mention {expected:?}, said {message:?}"
        );
    }
}

#[test]
fn validity_is_not_judged_here() {
    // An unclosed ring parses. It is refused when it reaches a record, after
    // snapping — judging it in two places would eventually be judging it
    // differently, and the boundary that snaps is the one that can see the shape
    // as it will actually be stored.
    shape_of("SET here:0 = geometry { type: 'Polygon', coordinates: [[[0, 0], [1, 0], [1, 1]]] };");
}

#[test]
fn a_member_of_a_collection_may_carry_its_own_marker() {
    // Redundant and is what a person writes. The canonical form — the one the
    // renderer emits — uses plain objects inside `geometries`, as RFC 7946 does;
    // both read as the same shape.
    let plain = shape_of(
        "SET here:0 = geometry { type: 'GeometryCollection', \
         geometries: [{ type: 'Point', coordinates: [1, 2] }] };",
    );
    let marked = shape_of(
        "SET here:0 = geometry { type: 'GeometryCollection', \
         geometries: [geometry { type: 'Point', coordinates: [1, 2] }] };",
    );
    assert_eq!(plain, marked);
}
