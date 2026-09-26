//! Distance from a position to a shape, checked against a brute-force oracle.
//!
//! The oracle walks every edge in small even steps and measures to each step
//! with the position-to-position distance. It shares nothing with the search
//! but that distance, and it cannot miss a minimum by more than half a step —
//! so the search must never come out **above** it (that would be a nearer point
//! the search skipped), and it can never come out below the true minimum, which
//! sits within half a step of the oracle's.
//!
//! The corpus spans scales on purpose: millimetres, metres, kilometres and a
//! thousand kilometres, at the equator, mid-latitude and close to a pole, since a
//! generator that produced one scale would certify defects at the others.

#![allow(clippy::unwrap_used)]
#![expect(
    clippy::cast_precision_loss,
    reason = "step counts and generator draws are small integers, far below 2^53"
)]

use tessari_geo::{Shape, Snapped, covers, distance, distance_to};
use tessari_types::{Geometry, Polygon, Position, Ring};

fn at(longitude: f64, latitude: f64) -> Snapped {
    Snapped::of(Position::new(longitude, latitude)).expect("on the sphere")
}

fn line(points: &[(f64, f64)]) -> Shape {
    let positions = points
        .iter()
        .map(|&(longitude, latitude)| Position::new(longitude, latitude))
        .collect();
    Shape::of(&Geometry::Line(positions)).unwrap()
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

/// The brute-force answer: every edge walked in `steps` pieces, plus every
/// vertex. Returns the least distance found and the longest step, in metres.
///
/// A position the shape covers is zero away. That is decided by the exact
/// predicate, which has its own oracle; walking edges alone would report the
/// distance to the boundary of an area the position is inside.
fn oracle(from: Snapped, shape: &Shape, steps: u32) -> (f64, f64) {
    if covers(shape, &Shape::Point(from)) {
        return (0.0, 0.0);
    }
    let mut best = f64::INFINITY;
    let mut longest = 0.0_f64;
    shape.each_position(&mut |vertex| {
        best = best.min(distance(from, vertex).unwrap());
    });
    shape.each_segment(&mut |start, end| {
        let (one, other) = (start.to_position(), end.to_position());
        let mut previous = one;
        for step in 1..=steps {
            let t = f64::from(step) / f64::from(steps);
            let here = Position::new(
                one.longitude + t * (other.longitude - one.longitude),
                one.latitude + t * (other.latitude - one.latitude),
            );
            let Ok(snapped) = Snapped::of(here) else {
                continue;
            };
            best = best.min(distance(from, snapped).unwrap());
            let walked = distance(Snapped::of(previous).unwrap(), snapped).unwrap();
            longest = longest.max(walked);
            previous = here;
        }
    });
    (best, longest)
}

/// Asserts the search against the oracle and returns the search's answer.
fn agrees(from: Snapped, shape: &Shape) -> f64 {
    let found = distance_to(from, shape).expect("nothing here is antipodal");
    let (brute, step) = oracle(from, shape, 4_000);
    // Snapping each oracle step moves it by at most half a grid unit.
    let snap = 1e-4;
    assert!(
        found <= brute + snap,
        "the search answered {found} m where a walk of the edges found {brute} m — it skipped a nearer point"
    );
    assert!(
        found >= brute - step / 2.0 - snap,
        "the search answered {found} m, below anything within half a step ({step} m) of the walk's {brute} m"
    );
    found
}

#[test]
fn a_covered_position_is_no_distance_away() {
    let square = polygon(&[&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]]);
    assert_eq!(distance_to(at(0.5, 0.5), &square), Some(0.0));
    assert_eq!(distance_to(at(1.0, 0.5), &square), Some(0.0), "on the edge");
    assert_eq!(distance_to(at(1.0, 1.0), &square), Some(0.0), "on a corner");
    let path = line(&[(0.0, 0.0), (2.0, 0.0)]);
    assert_eq!(distance_to(at(1.0, 0.0), &path), Some(0.0), "on a path");
}

#[test]
fn a_position_in_a_hole_is_measured_to_the_hole() {
    let holed = polygon(&[
        &[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0), (0.0, 0.0)],
        &[(1.0, 1.0), (3.0, 1.0), (3.0, 3.0), (1.0, 3.0), (1.0, 1.0)],
    ]);
    let found = agrees(at(2.0, 2.0), &holed);
    // The nearest part of the hole's ring is a whole degree of latitude or of
    // longitude away, somewhere near 111 km.
    assert!(found > 100_000.0 && found < 115_000.0, "{found}");
}

#[test]
fn the_nearest_point_of_an_edge_can_be_between_its_ends() {
    // The equator from 1°W to 1°E, and a position a degree north of its middle:
    // the nearest point is (0, 0), which is no vertex, and the distance is the
    // meridian arc — the position-to-position distance to that point.
    let equator = line(&[(-1.0, 0.0), (1.0, 0.0)]);
    let from = at(0.0, 1.0);
    let found = agrees(from, &equator);
    let arc = distance(from, at(0.0, 0.0)).unwrap();
    assert!((found - arc).abs() < 1e-3, "{found} against the arc {arc}");
    let corner = distance(from, at(1.0, 0.0)).unwrap();
    assert!(
        found < corner - 1_000.0,
        "a vertex answer would have been {corner}"
    );
}

#[test]
fn an_empty_shape_is_unreachably_far() {
    let empty = Shape::of(&Geometry::Collection(Vec::new())).unwrap();
    assert_eq!(distance_to(at(0.0, 0.0), &empty), Some(f64::INFINITY));
}

#[test]
fn a_multi_shape_answers_for_its_nearest_member() {
    let near = line(&[(10.0, 10.0), (10.5, 10.0)]);
    let far = line(&[(20.0, 10.0), (20.5, 10.0)]);
    let both = Shape::Collection(vec![far.clone(), near.clone()]);
    let from = at(10.2, 10.3);
    assert_eq!(distance_to(from, &both), distance_to(from, &near));
}

/// A small deterministic generator, so a failure names a case that reproduces.
struct Draws(u64);

impl Draws {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn within(&mut self, low: f64, high: f64) -> f64 {
        low + self.next() * (high - low)
    }
}

#[test]
fn the_search_agrees_with_the_walk_at_every_scale_and_latitude() {
    let mut draws = Draws(0x6765_6f5f_7265_6163);
    // How many cases measured a real distance rather than answering zero for a
    // covered position — a corpus that was all zeros would certify nothing.
    let mut apart = 0_u32;
    // (the latitude the shape sits at, the size of the shape in degrees, how far
    // off it the position is placed, in degrees)
    let scales: [(f64, f64, f64); 8] = [
        (0.0, 1e-6, 1e-8),
        (0.0, 1e-3, 1e-4),
        (48.0, 0.05, 0.01),
        (48.0, 1.0, 0.5),
        (-33.0, 5.0, 3.0),
        (78.0, 0.5, 0.2),
        (-85.0, 2.0, 1.0),
        (60.0, 8.0, 6.0),
    ];
    for (latitude, size, offset) in scales {
        for _ in 0..6 {
            let west = draws.within(-170.0, 160.0);
            let south = (latitude + draws.within(-size, 0.0)).clamp(-89.0, 89.0 - size);
            let corners: Vec<(f64, f64)> = (0..5)
                .map(|_| {
                    (
                        west + draws.within(0.0, size),
                        south + draws.within(0.0, size),
                    )
                })
                .collect();
            let path = line(&corners);
            let from = at(
                west + draws.within(-offset, size + offset),
                (south + draws.within(-offset, size + offset)).clamp(-89.9, 89.9),
            );
            if agrees(from, &path) > 0.0 {
                apart += 1;
            }
            let triangle = polygon(&[&[corners[0], corners[1], corners[2], corners[0]]]);
            if triangle_is_simple(&corners) && agrees(from, &triangle) > 0.0 {
                apart += 1;
            }
        }
    }
    assert!(
        apart >= 60,
        "only {apart} of 96 cases were apart from their shape"
    );
}

/// Whether three corners make a triangle with area, which is the only kind the
/// polygon builder here is asked to make.
fn triangle_is_simple(corners: &[(f64, f64)]) -> bool {
    let [(ax, ay), (bx, by), (cx, cy), ..] = corners else {
        return false;
    };
    ((bx - ax) * (cy - ay) - (by - ay) * (cx - ax)).abs() > 1e-12
}
