//! Distance between two shapes, checked against a brute-force oracle (G058 C1).
//!
//! The oracle walks every edge of **both** shapes in small even steps and
//! measures each step to the other shape with the position-to-shape distance,
//! which has an oracle of its own (`reach.rs`). It cannot miss the least distance
//! by more than half a step, and every figure it reports is a real distance; so
//! the answer must come out no more than [`TOLERANCE_METRES`] above it and no
//! more than half a step below it.

#![allow(clippy::unwrap_used)]

use tessari_geo::{Shape, Snapped, TOLERANCE_METRES, distance, distance_between, distance_to};
use tessari_types::{Geometry, Polygon, Position, Ring};

fn at(longitude: f64, latitude: f64) -> Snapped {
    Snapped::of(Position::new(longitude, latitude)).unwrap()
}

fn line(points: &[(f64, f64)]) -> Shape {
    Shape::of(&Geometry::Line(
        points
            .iter()
            .map(|&(longitude, latitude)| Position::new(longitude, latitude))
            .collect(),
    ))
    .unwrap()
}

fn polygon(rings: &[&[(f64, f64)]]) -> Shape {
    let mut rings = rings.iter().map(|corners| {
        Ring(
            corners
                .iter()
                .map(|&(longitude, latitude)| Position::new(longitude, latitude))
                .collect(),
        )
    });
    let exterior = rings.next().unwrap();
    Shape::of(&Geometry::Polygon(Polygon {
        exterior,
        interiors: rings.collect(),
    }))
    .unwrap()
}

fn square(west: f64, south: f64, side: f64) -> Vec<(f64, f64)> {
    vec![
        (west, south),
        (west + side, south),
        (west + side, south + side),
        (west, south + side),
        (west, south),
    ]
}

/// The least over sampled points of `one`'s edges and vertices, measured to
/// `other`, and the longest step taken.
fn sampled(one: &Shape, other: &Shape, steps: u32) -> (f64, f64) {
    let mut best = f64::INFINITY;
    let mut longest = 0.0_f64;
    one.each_position(&mut |vertex| {
        best = best.min(distance_to(vertex, other).unwrap());
    });
    one.each_segment(&mut |start, end| {
        let (from, to) = (start.to_position(), end.to_position());
        let mut previous = start;
        for step in 1..=steps {
            let t = f64::from(step) / f64::from(steps);
            let here = at(
                from.longitude + t * (to.longitude - from.longitude),
                from.latitude + t * (to.latitude - from.latitude),
            );
            longest = longest.max(distance(previous, here).unwrap());
            best = best.min(distance_to(here, other).unwrap());
            previous = here;
        }
    });
    (best, longest)
}

fn agrees(one: &Shape, other: &Shape, steps: u32) -> f64 {
    let answered = distance_between(one, other).unwrap();
    let swapped = distance_between(other, one).unwrap();
    let (forward, step) = sampled(one, other, steps);
    let (backward, back_step) = sampled(other, one, steps);
    let oracle = forward.min(backward);
    let slack = step.max(back_step) / 2.0;
    for figure in [answered, swapped] {
        assert!(
            figure <= oracle + TOLERANCE_METRES,
            "a nearer pair was skipped: {figure} against the oracle's {oracle}"
        );
        assert!(
            figure >= oracle - slack - TOLERANCE_METRES,
            "below every point the oracle measured: {figure} against {oracle} (step {step})"
        );
    }
    answered
}

#[test]
fn two_areas_a_kilometre_apart() {
    let paris = polygon(&[&square(2.30, 48.85, 0.01)]);
    let beside = polygon(&[&square(2.3236, 48.853, 0.01)]);
    let metres = agrees(&paris, &beside, 200);
    assert!((900.0..1000.0).contains(&metres), "{metres}");
}

#[test]
fn two_long_paths_near_a_pole_meet_nearest_inside_both() {
    // Lon–lat straight lines this long bend against the ellipsoid, so the
    // nearest pair need not be a vertex of either.
    let one = line(&[(-15.0, 70.0), (15.0, 70.0)]);
    let other = line(&[(-15.0, 72.0), (15.0, 70.5)]);
    agrees(&one, &other, 200);
    let skew = line(&[(-20.0, 71.0), (20.0, 71.2)]);
    agrees(&one, &skew, 200);
}

#[test]
fn an_area_inside_a_hole_is_as_far_as_the_hole_s_edge() {
    let holed = polygon(&[&square(10.0, 10.0, 1.0), &square(10.25, 10.25, 0.5)]);
    let inside = polygon(&[&square(10.4, 10.4, 0.1)]);
    let metres = agrees(&holed, &inside, 200);
    assert!(metres > 10_000.0, "{metres}");
}

#[test]
fn a_path_and_an_area_a_thousand_kilometres_apart() {
    let path = line(&[(0.0, 0.0), (5.0, 3.0), (9.0, -2.0)]);
    let area = polygon(&[&square(3.0, 10.0, 2.0)]);
    agrees(&path, &area, 2_000);
}

#[test]
fn shapes_that_share_a_point_are_zero_apart() {
    let one = polygon(&[&square(0.0, 0.0, 1.0)]);
    let touching = polygon(&[&square(1.0, 0.0, 1.0)]);
    let crossing = line(&[(-1.0, 0.5), (2.0, 0.5)]);
    let inside = polygon(&[&square(0.2, 0.2, 0.1)]);
    for other in [&touching, &crossing, &inside] {
        assert!(distance_between(&one, other).unwrap().abs() < f64::EPSILON);
    }
}

#[test]
fn a_position_measures_as_distance_to_does_and_an_empty_shape_is_unreachable() {
    let area = polygon(&[&square(3.0, 10.0, 2.0)]);
    let here = at(0.5, 0.5);
    let to = distance_to(here, &area).unwrap();
    let between = distance_between(&Shape::Point(here), &area).unwrap();
    assert!((to - between).abs() <= TOLERANCE_METRES, "{to} {between}");
    let empty = Shape::of(&Geometry::Collection(Vec::new())).unwrap();
    assert_eq!(distance_between(&area, &empty), Some(f64::INFINITY));
}
