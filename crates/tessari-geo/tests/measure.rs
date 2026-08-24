//! Distance and area, checked against oracles rather than against remembered
//! numbers.
//!
//! # Why not published test vectors
//!
//! Because a number recalled and typed into a test is a number that can be
//! recalled wrong, and a wrong control point makes a correct implementation look
//! broken or — far worse — makes a broken one look correct. Every expected value
//! below is either **derived from the datum's two defining constants** or
//! **computed by numerical integration inside the test**, from a formula that
//! shares no code with the thing being tested.
//!
//! The three oracles:
//!
//! - **The equator is a circle of radius `a`.** So the distance from 0°E to 90°E
//!   along it is exactly `a · π/2`, with no geodesy involved at all.
//! - **A meridian arc is an integral.** `∫ M(ψ) dψ` over the meridian radius of
//!   curvature, evaluated here by Simpson's rule. Vincenty's series must agree
//!   with it to a millimetre.
//! - **The ellipsoid's surface area is an integral too.** Which fixes the
//!   authalic radius independently of the closed form the implementation uses.
//!
//! Plus the properties that need no oracle: symmetry, zero to itself, and
//! monotonicity along a meridian.

#![allow(clippy::unwrap_used)]
#![expect(
    clippy::cast_precision_loss,
    reason = "an integration step count is a small literal, far below 2^53"
)]

use tessari_geo::{Shape, area, authalic_radius, distance};
use tessari_types::{Geometry, Polygon, Position, Ring};

/// WGS-84's semi-major axis. A **defining** constant of the datum, not a value
/// derived from anything, so restating it here is a restatement rather than a
/// second source of truth.
const SEMI_MAJOR: f64 = 6_378_137.0;

/// WGS-84's flattening, likewise defining.
const INVERSE_FLATTENING: f64 = 298.257_223_563;

fn at(longitude: f64, latitude: f64) -> tessari_geo::Snapped {
    tessari_geo::Snapped::of(Position::new(longitude, latitude)).expect("on the sphere")
}

fn metres(one: (f64, f64), other: (f64, f64)) -> f64 {
    distance(at(one.0, one.1), at(other.0, other.1)).expect("these are not antipodal")
}

fn squared_eccentricity() -> f64 {
    let flattening = 1.0 / INVERSE_FLATTENING;
    flattening * (2.0 - flattening)
}

/// Simpson's rule over `steps` intervals; `steps` must be even.
fn integrate(from: f64, to: f64, steps: usize, f: impl Fn(f64) -> f64) -> f64 {
    let width = (to - from) / steps as f64;
    let mut total = f(from) + f(to);
    for step in 1..steps {
        let at = from + width * step as f64;
        total += f(at) * if step % 2 == 0 { 2.0 } else { 4.0 };
    }
    total * width / 3.0
}

/// The meridian arc from the equator to `latitude`, in metres, by integration.
///
/// `M(ψ) = a(1 − e²) / (1 − e² sin²ψ)^{3/2}` — the meridian radius of curvature.
/// Shares no code with the implementation under test.
fn meridian_arc(latitude: f64) -> f64 {
    let squared = squared_eccentricity();
    integrate(0.0, latitude.to_radians(), 100_000, |psi| {
        SEMI_MAJOR * (1.0 - squared) / (1.0 - squared * psi.sin().powi(2)).powf(1.5)
    })
}

/// The ellipsoid's surface area, in square metres, by integration.
///
/// A surface of revolution: `S = 2π ∫ N(ψ) cos ψ · M(ψ) dψ`, which reduces to
/// `2π ∫ a²(1 − e²) cos ψ / (1 − e² sin²ψ)² dψ` over the two poles.
fn integrated_surface() -> f64 {
    let squared = squared_eccentricity();
    let half = core::f64::consts::FRAC_PI_2;
    core::f64::consts::TAU
        * integrate(-half, half, 100_000, |psi| {
            SEMI_MAJOR.powi(2) * (1.0 - squared) * psi.cos()
                / (1.0 - squared * psi.sin().powi(2)).powi(2)
        })
}

// ------------------------------------------------------------------ distance

#[test]
fn along_the_equator_the_answer_is_the_arc_of_a_circle_of_radius_a() {
    // The equator is the one line on the ellipsoid whose length needs no
    // geodesy: it is a circle of radius `a`, so a quarter of it is `a·π/2`.
    let quarter = SEMI_MAJOR * core::f64::consts::FRAC_PI_2;
    assert!(
        (metres((0.0, 0.0), (90.0, 0.0)) - quarter).abs() < 1e-3,
        "a quarter of the equator should be {quarter} m, got {}",
        metres((0.0, 0.0), (90.0, 0.0))
    );

    let eighth = SEMI_MAJOR * core::f64::consts::FRAC_PI_4;
    assert!((metres((10.0, 0.0), (55.0, 0.0)) - eighth).abs() < 1e-3);
}

#[test]
fn along_a_meridian_the_answer_matches_an_integrated_arc() {
    // Two independent computations of the same length: a series solution and a
    // numerical integral of the meridian radius of curvature.
    for latitude in [1.0, 10.0, 45.0, 60.0, 89.0] {
        let integrated = meridian_arc(latitude);
        let measured = metres((0.0, 0.0), (0.0, latitude));
        assert!(
            (measured - integrated).abs() < 1e-3,
            "at {latitude}°, integration says {integrated} m and the series says {measured} m"
        );
    }
}

#[test]
fn a_meridian_arc_between_two_latitudes_is_the_difference_of_two_arcs() {
    let between = metres((0.0, 20.0), (0.0, 50.0));
    let difference = meridian_arc(50.0) - meridian_arc(20.0);
    assert!((between - difference).abs() < 1e-3);
}

#[test]
fn distance_is_symmetric_and_nothing_to_itself() {
    let paris = (2.294_481, 48.858_37);
    let lyon = (4.835_659, 45.764_043);
    assert_eq!(metres(paris, lyon), metres(lyon, paris));
    assert_eq!(metres(paris, paris), 0.0);
}

#[test]
fn distance_grows_as_the_second_position_moves_away() {
    let mut previous = 0.0;
    for latitude in [1.0, 2.0, 5.0, 20.0, 60.0, 80.0] {
        let now = metres((0.0, 0.0), (0.0, latitude));
        assert!(now > previous, "{latitude}° is not further than the last");
        previous = now;
    }
}

#[test]
fn a_near_antipodal_pair_is_refused_rather_than_guessed() {
    // Vincenty's inverse oscillates here instead of converging. The last iterate
    // is a plausible twenty-thousand-kilometre number wrong by an amount nobody
    // can bound, so it is not returned.
    let refused = distance(at(0.0, 0.0), at(179.7, 0.5));
    assert_eq!(
        refused, None,
        "a near-antipodal pair should be refused, got {refused:?}"
    );

    // And the refusal is narrow: a pair that is merely very far apart still
    // answers, so this is not a whole hemisphere going dark.
    assert!(distance(at(0.0, 0.0), at(150.0, 40.0)).is_some());
}

// ---------------------------------------------------------------------- area

#[test]
fn the_authalic_radius_reproduces_the_integrated_surface_area() {
    // The implementation uses the closed form for an oblate spheroid. This
    // checks it against the integral of the same surface, which is a different
    // computation of the same quantity.
    let radius = authalic_radius();
    let from_radius = 4.0 * core::f64::consts::PI * radius * radius;
    let integrated = integrated_surface();
    let relative = (from_radius - integrated).abs() / integrated;
    assert!(
        relative < 1e-9,
        "closed form {from_radius} m² against integral {integrated} m², relative {relative}"
    );
}

fn ring(corners: &[(f64, f64)]) -> Ring {
    let mut positions: Vec<Position> = corners
        .iter()
        .map(|&(longitude, latitude)| Position::new(longitude, latitude))
        .collect();
    positions.push(positions[0]);
    Ring(positions)
}

fn shape(geometry: &Geometry) -> Shape {
    Shape::of(geometry).expect("on the sphere")
}

fn box_shape(west: f64, south: f64, east: f64, north: f64) -> Shape {
    shape(&Geometry::Polygon(Polygon {
        exterior: ring(&[(west, south), (east, south), (east, north), (west, north)]),
        interiors: Vec::new(),
    }))
}

/// The exact area of a longitude–latitude box on a sphere of the given radius:
/// `R² · Δλ · (sin φ₂ − sin φ₁)`. Closed form, no integration, no shared code.
fn box_area(west: f64, south: f64, east: f64, north: f64) -> f64 {
    let radius = authalic_radius();
    radius
        * radius
        * (east - west).to_radians()
        * (north.to_radians().sin() - south.to_radians().sin())
}

#[test]
fn a_longitude_latitude_box_matches_its_closed_form() {
    for (west, south, east, north) in [
        (0.0, 0.0, 1.0, 1.0),
        (-10.0, -5.0, 10.0, 5.0),
        (100.0, 55.0, 140.0, 70.0),
        (-180.0, -80.0, 0.0, 80.0),
    ] {
        let expected = box_area(west, south, east, north);
        let measured = area(&box_shape(west, south, east, north));
        let relative = (measured - expected).abs() / expected;
        assert!(
            relative < 1e-9,
            "box {west},{south}..{east},{north}: closed form {expected} m², measured {measured} m²"
        );
    }
}

#[test]
fn the_area_is_the_same_whichever_way_round_the_ring_was_written() {
    let clockwise = shape(&Geometry::Polygon(Polygon {
        exterior: ring(&[(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)]),
        interiors: Vec::new(),
    }));
    let counter = box_shape(0.0, 0.0, 1.0, 1.0);
    assert!((area(&clockwise) - area(&counter)).abs() < 1e-6);
}

#[test]
fn a_hole_is_taken_out_of_the_area_it_sits_in() {
    let holed = shape(&Geometry::Polygon(Polygon {
        exterior: ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]),
        interiors: vec![ring(&[(2.0, 2.0), (6.0, 2.0), (6.0, 6.0), (2.0, 6.0)])],
    }));
    let expected = box_area(0.0, 0.0, 10.0, 10.0) - box_area(2.0, 2.0, 6.0, 6.0);
    let relative = (area(&holed) - expected).abs() / expected;
    assert!(relative < 1e-9, "{} against {expected}", area(&holed));
}

#[test]
fn a_shape_with_no_interior_covers_no_ground() {
    assert_eq!(area(&shape(&Geometry::Point(Position::new(3.0, 4.0)))), 0.0);
    assert_eq!(
        area(&shape(&Geometry::Line(vec![
            Position::new(0.0, 0.0),
            Position::new(1.0, 1.0),
        ]))),
        0.0
    );
    assert_eq!(area(&shape(&Geometry::Collection(Vec::new()))), 0.0);
}

#[test]
fn the_members_of_a_multi_polygon_add_up() {
    let two = shape(&Geometry::MultiPolygon(vec![
        Polygon {
            exterior: ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]),
            interiors: Vec::new(),
        },
        Polygon {
            exterior: ring(&[(5.0, 5.0), (7.0, 5.0), (7.0, 7.0), (5.0, 7.0)]),
            interiors: Vec::new(),
        },
    ]));
    let expected = box_area(0.0, 0.0, 1.0, 1.0) + box_area(5.0, 5.0, 7.0, 7.0);
    assert!((area(&two) - expected).abs() / expected < 1e-9);
}

#[test]
fn the_same_ground_measures_the_same_wherever_it_is_written_from() {
    // Longitude is a free parameter for an area: a box of the same size at the
    // same latitudes covers the same ground whether it is written at 0° or 170°.
    // A wrapping bug in the longitude span would break this and nothing else.
    let here = area(&box_shape(0.0, 40.0, 5.0, 45.0));
    let there = area(&box_shape(170.0, 40.0, 175.0, 45.0));
    assert!((here - there).abs() / here < 1e-12);
}
