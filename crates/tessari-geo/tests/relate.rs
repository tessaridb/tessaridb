//! How two shapes stand to each other.
//!
//! # Every answer below was decided before the code ran
//!
//! That is not a style preference. Every geospatial failure is silent — there is
//! no exception and no NaN, and a wrong answer is a position on the right street
//! in the right city that happens to be outside the park. A fixture whose
//! expected value was read off the implementation proves the implementation
//! agrees with itself, which is the one thing that was never in doubt.
//!
//! So each case here names the geometry in words, states the answer the
//! definition gives, and only then asks the code.
//!
//! # The two cases that carry the design
//!
//! **`contains` and `covers` must disagree on a boundary position.** If they
//! ever agree everywhere, one of them has quietly become the other and the
//! DE-9IM distinction — a boundary is a real place, not a rounding of inside —
//! has been lost. There is a test whose whole job is to fail if that happens.
//!
//! **A segment is cut at every touch, not tested at its middle.** The comb
//! fixture below crosses two notches and one solid tooth, and the midpoint of
//! the *whole* segment lands on the tooth. An implementation that tested one
//! midpoint would call it covered. The right answer is that it is not.

use tessari_geo::{Shape, contains, covered_by, covers, disjoint, equals, intersects, within};
use tessari_types::{Geometry, Polygon, Position, Ring};

fn at(longitude: f64, latitude: f64) -> Position {
    Position::new(longitude, latitude)
}

fn closed(corners: &[(f64, f64)]) -> Ring {
    let mut positions: Vec<Position> = corners
        .iter()
        .map(|&(longitude, latitude)| at(longitude, latitude))
        .collect();
    if let Some(first) = positions.first().copied() {
        positions.push(first);
    }
    Ring(positions)
}

fn lowered(geometry: &Geometry) -> Shape {
    Shape::of(geometry).expect("every fixture position is on the sphere")
}

fn point(longitude: f64, latitude: f64) -> Shape {
    lowered(&Geometry::Point(at(longitude, latitude)))
}

fn line(corners: &[(f64, f64)]) -> Shape {
    lowered(&Geometry::Line(
        corners
            .iter()
            .map(|&(longitude, latitude)| at(longitude, latitude))
            .collect(),
    ))
}

fn area(shell: &[(f64, f64)], holes: &[&[(f64, f64)]]) -> Shape {
    lowered(&Geometry::Polygon(Polygon {
        exterior: closed(shell),
        interiors: holes.iter().map(|hole| closed(hole)).collect(),
    }))
}

/// A square from `(low, low)` to `(high, high)`, counter-clockwise.
fn square(low: f64, high: f64) -> Shape {
    area(&[(low, low), (high, low), (high, high), (low, high)], &[])
}

/// A square with a square bite out of its middle.
fn square_with_hole() -> Shape {
    area(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[&[(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0)]],
    )
}

/// A comb: a ten-wide square with two notches cut down from its top edge, at
/// `x ∈ [2, 4]` and `x ∈ [6, 8]`, leaving a solid tooth between them.
///
/// The fixture the subdivision rule exists for.
fn comb() -> Shape {
    area(
        &[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (8.0, 10.0),
            (8.0, 4.0),
            (6.0, 4.0),
            (6.0, 10.0),
            (4.0, 10.0),
            (4.0, 4.0),
            (2.0, 4.0),
            (2.0, 10.0),
            (0.0, 10.0),
        ],
        &[],
    )
}

// ------------------------------------------------------------------ positions

#[test]
fn a_position_inside_a_square_is_intersecting_covered_and_contained() {
    let (here, inside) = (square(0.0, 10.0), point(5.0, 5.0));
    assert!(intersects(&here, &inside));
    assert!(!disjoint(&here, &inside));
    assert!(covers(&here, &inside));
    assert!(contains(&here, &inside));
    assert!(within(&inside, &here));
    assert!(covered_by(&inside, &here));
}

#[test]
fn a_position_on_an_edge_is_covered_and_not_contained() {
    // The one distinction the whole predicate layer is shaped around. A position
    // on a boundary is in the shape and not in its interior, so `covers` says
    // yes and `contains` says no — and if these two ever agree here, one of them
    // has silently become the other.
    let (here, on_the_edge) = (square(0.0, 10.0), point(0.0, 5.0));
    assert!(intersects(&here, &on_the_edge));
    assert!(covers(&here, &on_the_edge));
    assert!(!contains(&here, &on_the_edge));
    assert!(covered_by(&on_the_edge, &here));
    assert!(!within(&on_the_edge, &here));
}

#[test]
fn a_position_at_a_corner_is_covered_and_not_contained() {
    let (here, corner) = (square(0.0, 10.0), point(10.0, 10.0));
    assert!(covers(&here, &corner));
    assert!(!contains(&here, &corner));
}

#[test]
fn a_position_outside_is_disjoint_from_the_square() {
    let (here, away) = (square(0.0, 10.0), point(11.0, 5.0));
    assert!(!intersects(&here, &away));
    assert!(disjoint(&here, &away));
    assert!(!covers(&here, &away));
}

#[test]
fn a_position_in_a_hole_is_outside_the_polygon_that_has_the_hole() {
    let here = square_with_hole();
    let in_the_hole = point(5.0, 5.0);
    assert!(disjoint(&here, &in_the_hole));
    assert!(!covers(&here, &in_the_hole));

    // On the hole's edge is on the polygon's boundary: a hole's ring is one.
    let on_the_holes_edge = point(3.0, 5.0);
    assert!(covers(&here, &on_the_holes_edge));
    assert!(!contains(&here, &on_the_holes_edge));
}

// ---------------------------------------------------------------------- paths

#[test]
fn a_path_crossing_a_square_meets_it_and_is_not_covered_by_it() {
    let here = square(0.0, 10.0);
    let through = line(&[(-5.0, 5.0), (15.0, 5.0)]);
    assert!(intersects(&here, &through));
    assert!(!covers(&here, &through));
}

#[test]
fn a_path_inside_a_square_is_covered_and_contained() {
    let here = square(0.0, 10.0);
    let inside = line(&[(2.0, 2.0), (8.0, 8.0)]);
    assert!(covers(&here, &inside));
    assert!(contains(&here, &inside));
}

#[test]
fn a_path_lying_along_an_edge_is_covered_and_not_contained() {
    let here = square(0.0, 10.0);
    let along = line(&[(0.0, 2.0), (0.0, 8.0)]);
    assert!(covers(&here, &along));
    // Every position of the path is on the square's boundary, so it is covered
    // by the square and not contained in it.
    assert!(!contains(&here, &along));
}

#[test]
fn a_path_that_leaves_and_re_enters_is_not_covered_even_when_its_middle_is_inside() {
    // The comb has notches at x ∈ [2, 4] and x ∈ [6, 8], and a solid tooth
    // between them. A segment along the top from (2, 10) to (8, 10) is:
    //   x ∈ [2, 4]  outside — the first notch's mouth
    //   x ∈ [4, 6]  on the boundary — the tooth's top edge
    //   x ∈ [6, 8]  outside — the second notch's mouth
    // Its overall midpoint is (5, 10), which is on the tooth. An implementation
    // testing one midpoint per segment would answer "covered" here, and would be
    // wrong twice over the same segment.
    let here = comb();
    let across = line(&[(2.0, 10.0), (8.0, 10.0)]);
    assert!(!covers(&here, &across));
    assert!(intersects(&here, &across));

    // The tooth's own top edge, on the other hand, is covered.
    let on_the_tooth = line(&[(4.0, 10.0), (6.0, 10.0)]);
    assert!(covers(&here, &on_the_tooth));
    assert!(!contains(&here, &on_the_tooth));
}

// ---------------------------------------------------------------------- areas

#[test]
fn a_square_contains_a_smaller_square_wholly_inside_it() {
    let (outer, inner) = (square(0.0, 10.0), square(2.0, 8.0));
    assert!(intersects(&outer, &inner));
    assert!(covers(&outer, &inner));
    assert!(contains(&outer, &inner));
    assert!(!covers(&inner, &outer));
    assert!(within(&inner, &outer));
}

#[test]
fn two_squares_sharing_an_edge_meet_and_neither_holds_the_other() {
    let west = square(0.0, 5.0);
    let east = area(&[(5.0, 0.0), (10.0, 0.0), (10.0, 5.0), (5.0, 5.0)], &[]);
    assert!(intersects(&west, &east));
    assert!(!covers(&west, &east));
    assert!(!covers(&east, &west));
}

#[test]
fn two_squares_apart_are_disjoint() {
    let west = square(0.0, 1.0);
    let east = area(&[(5.0, 0.0), (6.0, 0.0), (6.0, 1.0), (5.0, 1.0)], &[]);
    assert!(disjoint(&west, &east));
    assert!(!covers(&west, &east));
}

#[test]
fn two_overlapping_squares_meet_and_neither_holds_the_other() {
    let west = square(0.0, 6.0);
    let east = area(&[(4.0, 0.0), (10.0, 0.0), (10.0, 6.0), (4.0, 6.0)], &[]);
    assert!(intersects(&west, &east));
    assert!(!covers(&west, &east));
    assert!(!covers(&east, &west));
}

#[test]
fn a_square_with_a_hole_does_not_cover_the_square_that_fills_the_hole() {
    // The hard case, and the one a boundary-only test gets wrong. The filling
    // square's whole boundary lies on the holed square's boundary, so every
    // position and every segment of it passes. What it does not pass is the
    // hole's own interior, which belongs to neither shape's owner: the hole is
    // outside the holed square, and inside the filler.
    let holed = square_with_hole();
    let filler = square(3.0, 7.0);
    assert!(!covers(&holed, &filler));
    assert!(intersects(&holed, &filler));

    // A shape beside the hole is covered as usual, so the hole check has not
    // simply refused everything.
    let beside = area(&[(0.5, 0.5), (2.5, 0.5), (2.5, 2.5), (0.5, 2.5)], &[]);
    assert!(covers(&holed, &beside));
    assert!(contains(&holed, &beside));
}

#[test]
fn a_shape_inside_a_hole_is_disjoint_from_the_shape_that_has_the_hole() {
    let holed = square_with_hole();
    let inside_the_hole = square(4.0, 6.0);
    assert!(disjoint(&holed, &inside_the_hole));
}

#[test]
fn a_holed_square_covers_another_holed_square_with_the_same_hole() {
    let outer = area(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[&[(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0)]],
    );
    let inner = area(
        &[(1.0, 1.0), (9.0, 1.0), (9.0, 9.0), (1.0, 9.0)],
        &[&[(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0)]],
    );
    assert!(covers(&outer, &inner));
    assert!(!covers(&inner, &outer));
}

// ------------------------------------------------------------- the rest of it

#[test]
fn a_shape_equals_itself_written_with_an_extra_collinear_vertex() {
    // Equality is about the positions covered, not about how the shape was
    // typed. A vertex added in the middle of an edge changes the text and
    // nothing else.
    let plain = square(0.0, 10.0);
    let with_a_spare_vertex = area(
        &[
            (0.0, 0.0),
            (5.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ],
        &[],
    );
    assert!(equals(&plain, &with_a_spare_vertex));
    assert!(equals(&with_a_spare_vertex, &plain));
}

#[test]
fn a_shape_does_not_equal_a_different_shape() {
    assert!(!equals(&square(0.0, 10.0), &square(0.0, 9.0)));
    assert!(!equals(&square(0.0, 10.0), &point(5.0, 5.0)));
}

#[test]
fn a_multi_shape_is_covered_only_when_every_one_of_its_parts_is() {
    let here = square(0.0, 10.0);
    let both_inside = lowered(&Geometry::MultiPoint(vec![at(2.0, 2.0), at(8.0, 8.0)]));
    let one_outside = lowered(&Geometry::MultiPoint(vec![at(2.0, 2.0), at(20.0, 8.0)]));
    assert!(covers(&here, &both_inside));
    assert!(!covers(&here, &one_outside));
    // And it still meets the square, because one of its positions does.
    assert!(intersects(&here, &one_outside));
}

#[test]
fn a_collection_is_covered_when_each_member_is() {
    let here = square(0.0, 10.0);
    let mixed = Geometry::Collection(vec![
        Box::new(Geometry::Point(at(1.0, 1.0))),
        Box::new(Geometry::Line(vec![at(2.0, 2.0), at(3.0, 3.0)])),
    ]);
    assert!(covers(&here, &lowered(&mixed)));

    let straying = Geometry::Collection(vec![
        Box::new(Geometry::Point(at(1.0, 1.0))),
        Box::new(Geometry::Line(vec![at(2.0, 2.0), at(30.0, 3.0)])),
    ]);
    assert!(!covers(&here, &lowered(&straying)));
}

#[test]
fn a_shape_holding_nothing_meets_nothing_and_is_inside_everything() {
    let here = square(0.0, 10.0);
    let nothing = lowered(&Geometry::MultiPoint(Vec::new()));
    // It has no position, so there is no position it shares with anything.
    assert!(disjoint(&here, &nothing));
    assert!(!intersects(&nothing, &nothing));
    // And no position of it is outside anything, which is what covering means.
    assert!(covers(&here, &nothing));
    // But it contains nothing, because there is no interior to meet.
    assert!(!contains(&here, &nothing));
    assert!(!covers(&nothing, &here));
}

#[test]
fn a_path_cannot_hold_an_area_however_much_of_its_edge_it_lies_along() {
    // A one-dimensional shape has no area to hold one in. The square's own
    // boundary, offered as a path, covers every position of the square's
    // boundary and still does not cover the square.
    let here = square(0.0, 10.0);
    let its_edge = line(&[
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ]);
    assert!(covers(&here, &its_edge));
    assert!(!covers(&its_edge, &here));
}

#[test]
fn covering_is_reflexive_and_containing_is_too_for_a_shape_with_an_interior() {
    for shape in [
        square(0.0, 10.0),
        square_with_hole(),
        comb(),
        line(&[(0.0, 0.0), (1.0, 1.0)]),
        point(3.0, 4.0),
    ] {
        assert!(covers(&shape, &shape), "a shape covers itself");
        assert!(equals(&shape, &shape));
        assert!(intersects(&shape, &shape));
    }
}
