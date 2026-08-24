//! Lowering a geometry to the grid, and the box around it.
//!
//! Two properties, and the second is the one that is easy to get almost right.
//!
//! The first is that **every one of the seven RFC 7946 forms lowers**, including
//! the nested ones. A form that was forgotten would not fail to compile — the
//! conversion matches on the enum, so it would fail to compile — but a form that
//! lowered *incompletely*, dropping a hole or a collection member, would not.
//! So each case below counts what came out.
//!
//! The second is that **the box around nothing is nothing**. A shape holding no
//! position has no smallest rectangle, and a degenerate box at the origin would
//! put an empty shape off the coast of Africa, where it would answer queries.

use tessari_geo::grid::{Axis, OffGrid};
use tessari_geo::{SCALE, Shape};
use tessari_types::{Geometry, Polygon, Position, Ring};

fn at(longitude: f64, latitude: f64) -> Position {
    Position::new(longitude, latitude)
}

fn ring(corners: &[(f64, f64)]) -> Ring {
    let mut positions: Vec<Position> = corners
        .iter()
        .map(|&(longitude, latitude)| at(longitude, latitude))
        .collect();
    if let Some(first) = positions.first().copied() {
        positions.push(first);
    }
    Ring(positions)
}

fn polygon(shell: &[(f64, f64)], holes: &[&[(f64, f64)]]) -> Polygon {
    Polygon {
        exterior: ring(shell),
        interiors: holes.iter().map(|hole| ring(hole)).collect(),
    }
}

fn lowered(geometry: &Geometry) -> Shape {
    Shape::of(geometry).expect("every position is on the sphere")
}

fn positions(shape: &Shape) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    shape.each_position(&mut |at| out.push((at.longitude_units(), at.latitude_units())));
    out
}

fn units(degrees: f64) -> i64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a test coordinate is a small integral number of degrees"
    )]
    {
        (degrees * SCALE) as i64
    }
}

#[test]
fn a_point_lowers_to_one_grid_position_longitude_first() {
    let shape = lowered(&Geometry::Point(at(2.35, 48.85)));
    assert_eq!(positions(&shape), vec![(units(2.35), units(48.85))]);
}

#[test]
fn a_line_keeps_its_positions_in_order() {
    let shape = lowered(&Geometry::Line(vec![at(0.0, 0.0), at(1.0, 2.0)]));
    assert_eq!(positions(&shape), vec![(0, 0), (units(1.0), units(2.0))]);
}

#[test]
fn a_polygon_lowers_its_shell_and_every_hole() {
    let shape = lowered(&Geometry::Polygon(polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[
            &[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0)],
            &[(5.0, 5.0), (6.0, 5.0), (6.0, 6.0), (5.0, 6.0)],
        ],
    )));
    // Five positions per closed ring, three rings. A hole silently dropped would
    // leave fifteen at ten, and every containment answer about that polygon
    // would then be wrong in a way that looks entirely reasonable.
    assert_eq!(positions(&shape).len(), 15);
}

#[test]
fn a_multipolygon_and_a_collection_lower_every_member() {
    let square = polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)], &[]);
    let multi = Geometry::MultiPolygon(vec![square.clone(), square.clone()]);
    assert_eq!(positions(&lowered(&multi)).len(), 10);

    let collection = Geometry::Collection(vec![
        Box::new(Geometry::Point(at(0.0, 0.0))),
        Box::new(Geometry::MultiPoint(vec![at(1.0, 1.0), at(2.0, 2.0)])),
        Box::new(multi),
    ]);
    assert_eq!(positions(&lowered(&collection)).len(), 13);
}

#[test]
fn a_multiline_keeps_its_paths_apart() {
    let shape = lowered(&Geometry::MultiLine(vec![
        vec![at(0.0, 0.0), at(1.0, 0.0)],
        vec![at(5.0, 5.0), at(6.0, 5.0), at(7.0, 5.0)],
    ]));
    let mut segments = Vec::new();
    shape.each_segment(&mut |from, to| segments.push((from, to)));
    // Three segments, not four: the gap between the two paths is not an edge.
    // A flattening that lost the boundary between them would join Corsica to
    // Sardinia and never say so.
    assert_eq!(segments.len(), 3);
}

#[test]
fn a_position_off_the_sphere_is_refused_rather_than_clamped() {
    let refused = Shape::of(&Geometry::Point(at(181.0, 0.0)));
    assert!(matches!(
        refused,
        Err(OffGrid::OutOfRange {
            axis: Axis::Longitude,
            ..
        })
    ));
}

#[test]
fn a_position_that_is_not_a_number_is_refused() {
    let refused = Shape::of(&Geometry::Point(at(0.0, f64::NAN)));
    assert!(matches!(
        refused,
        Err(OffGrid::NotFinite {
            axis: Axis::Latitude,
            ..
        })
    ));
}

#[test]
fn the_box_is_the_smallest_rectangle_holding_every_position() {
    let shape = lowered(&Geometry::MultiPoint(vec![
        at(-3.0, 4.0),
        at(7.0, -2.0),
        at(1.0, 1.0),
    ]));
    let bounds = shape.bounds().expect("three positions have a box");
    assert_eq!(bounds.west(), units(-3.0));
    assert_eq!(bounds.south(), units(-2.0));
    assert_eq!(bounds.east(), units(7.0));
    assert_eq!(bounds.north(), units(4.0));
}

#[test]
fn the_box_around_nothing_is_nothing() {
    for empty in [
        Geometry::MultiPoint(Vec::new()),
        Geometry::Collection(Vec::new()),
        Geometry::Collection(vec![Box::new(Geometry::MultiLine(Vec::new()))]),
    ] {
        let shape = lowered(&empty);
        assert_eq!(shape.bounds(), None, "{empty:?} should have no box");
    }
}

#[test]
fn the_boundary_of_an_area_is_its_rings_and_the_boundary_of_a_path_is_its_ends() {
    let area = lowered(&Geometry::Polygon(polygon(
        &[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)],
        &[&[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0)]],
    )));
    assert_eq!(positions(&area.boundary()).len(), 10);

    let path = lowered(&Geometry::Line(vec![
        at(0.0, 0.0),
        at(1.0, 1.0),
        at(2.0, 0.0),
    ]));
    assert_eq!(positions(&path.boundary()).len(), 2);

    // A closed path has no ends. It is drawn as a line, so it bounds no area
    // either — but a predicate that handed it two coincident boundary positions
    // would report a ring as touching itself at a place that is not a boundary.
    let closed = lowered(&Geometry::Line(vec![
        at(0.0, 0.0),
        at(1.0, 1.0),
        at(2.0, 0.0),
        at(0.0, 0.0),
    ]));
    assert_eq!(positions(&closed.boundary()).len(), 0);
}

#[test]
fn a_position_has_no_boundary() {
    let point = lowered(&Geometry::Point(at(1.0, 1.0)));
    assert_eq!(positions(&point.boundary()).len(), 0);
}
