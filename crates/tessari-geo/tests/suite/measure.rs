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

// ---------------------------------------------------------------------------
// The floor a nearest-first traversal orders its frontier by.
//
// `no_closer_than` is not checked against a remembered number or against itself.
// It is checked against `distance` — a different function, on a different
// formula — evaluated at many positions inside the box. The claim under test is
// one-directional and that is the whole point: the bound may be smaller than
// every distance it bounds, and it may never be larger than one of them.
// ---------------------------------------------------------------------------

/// Grid units in one degree.
const DEGREE: i64 = 1_000_000_000;

/// Half the world in longitude, which is also the whole of it in latitude.
const HALF_WORLD: i64 = 180 * DEGREE;

/// The whole world in longitude.
const WHOLE_WORLD: i64 = 360 * DEGREE;

fn units(longitude: i64, latitude: i64) -> tessari_geo::Snapped {
    tessari_geo::Snapped::from_units(longitude, latitude).expect("on the grid")
}

fn box_of(west: i64, south: i64, east: i64, north: i64) -> tessari_geo::Bounds {
    tessari_geo::Bounds::of_position(units(west, south)).widened_to(units(east, north))
}

/// A deterministic sequence, so a failure is reproducible from its own seed.
struct Rolls(u64);

impl Rolls {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A value in `0..span`, or zero for an empty span.
    fn upto(&mut self, span: u64) -> i64 {
        let drawn = self.next().checked_rem(span.max(1)).unwrap_or(0);
        i64::try_from(drawn).unwrap_or(0)
    }

    /// A box of the given side placed so it stays on the planet, drawn from the
    /// room **left** after the side is taken rather than drawn freely and
    /// clamped — clamping would pile every box against the poles, which is the
    /// one place the bound behaves least like it does everywhere else.
    fn box_somewhere(&mut self, side: i64) -> tessari_geo::Bounds {
        let side = side.min(HALF_WORLD / 2);
        let east_room = u64::try_from(WHOLE_WORLD.saturating_sub(side) / DEGREE).unwrap_or(1);
        let north_room = u64::try_from(HALF_WORLD.saturating_sub(side) / DEGREE).unwrap_or(1);
        let west = self
            .upto(east_room)
            .saturating_mul(DEGREE)
            .saturating_sub(HALF_WORLD);
        let south = self
            .upto(north_room)
            .saturating_mul(DEGREE)
            .saturating_sub(HALF_WORLD / 2);
        box_of(
            west,
            south,
            west.saturating_add(side),
            south.saturating_add(side),
        )
    }

    fn position_somewhere(&mut self) -> tessari_geo::Snapped {
        units(
            self.upto(360)
                .saturating_mul(DEGREE)
                .saturating_sub(HALF_WORLD),
            self.upto(179)
                .saturating_mul(DEGREE)
                .saturating_sub(89 * DEGREE),
        )
    }
}

/// Every distance from `from` to a lattice of positions across the box, plus its
/// corners and the position nearest by coordinate clamping.
///
/// Near-antipodal pairs are dropped rather than counted, because `distance`
/// refuses those by design; the caller asserts that something survived.
fn probed_distances(from: tessari_geo::Snapped, area: tessari_geo::Bounds) -> Vec<f64> {
    const STEPS: i64 = 12;
    let across = area
        .east()
        .saturating_sub(area.west())
        .checked_div(STEPS)
        .unwrap_or(0);
    let up = area
        .north()
        .saturating_sub(area.south())
        .checked_div(STEPS)
        .unwrap_or(0);
    let mut probes = Vec::new();
    for step_x in 0..=STEPS {
        for step_y in 0..=STEPS {
            probes.push(units(
                area.west().saturating_add(across.saturating_mul(step_x)),
                area.south().saturating_add(up.saturating_mul(step_y)),
            ));
        }
    }
    // The corner or edge point a coordinate clamp lands on — for a box in
    // longitude and latitude this is where the true nearest position very nearly
    // is, so a lattice that missed it would make the check weaker than it looks.
    probes.push(units(
        from.longitude_units().clamp(area.west(), area.east()),
        from.latitude_units().clamp(area.south(), area.north()),
    ));
    probes
        .into_iter()
        .filter_map(|probe| distance(from, probe))
        .collect()
}

#[test]
fn the_bound_is_never_larger_than_a_distance_it_bounds() {
    // The property the traversal's correctness rests on, over five scales of box
    // and a spread of query positions. A bound that exceeded a real distance
    // would let a walk discard the region holding the nearest record.
    let mut rolls = Rolls(0x9e37_79b9_7f4a_7c15);
    let mut checked = 0_usize;
    for side_degrees in [1_i64, 3, 10, 40, 90] {
        let side = side_degrees * DEGREE;
        for _ in 0..40 {
            let area = rolls.box_somewhere(side);
            let from = rolls.position_somewhere();
            let bound = tessari_geo::no_closer_than(from, area);
            let probes = probed_distances(from, area);
            assert!(
                !probes.is_empty(),
                "every probe was refused as near-antipodal, so nothing was checked"
            );
            for probe in probes {
                assert!(
                    bound <= probe + 1e-6,
                    "the bound {bound} exceeds a real distance {probe} \
                     from {from:?} to {area:?} at side {side_degrees}°"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 10_000, "only {checked} probes were compared");
}

#[test]
fn a_position_inside_the_box_is_bounded_by_nothing() {
    // Zero is the only correct answer for a position the box holds, edges and
    // corners included: the box contains it, so nothing in the box is further
    // from it than zero.
    let area = box_of(-10 * DEGREE, 20 * DEGREE, 30 * DEGREE, 50 * DEGREE);
    for (longitude, latitude) in [
        (0_i64, 30_i64),
        (-10, 20),
        (30, 50),
        (-10, 50),
        (30, 20),
        (-10, 35),
        (30, 35),
        (5, 20),
        (5, 50),
    ] {
        let inside = units(longitude * DEGREE, latitude * DEGREE);
        assert_eq!(
            tessari_geo::no_closer_than(inside, area),
            0.0,
            "the box holds {longitude}°, {latitude}° and must bound it by nothing"
        );
    }
}

#[test]
fn the_bound_is_tight_where_a_traversal_needs_it() {
    // A floor that is always zero is a correct floor and a useless one, so the
    // bound is exercised where a walk actually leans on it — a small box a short
    // way from the query, which is the situation at the frontier when the answer
    // is nearly settled. The aggregate is not enough: one slack scale hiding
    // behind four tight ones is exactly the failure this asserts per scale.
    let mut rolls = Rolls(0x5ca1_e50d_0e5c_3d00);
    for side_degrees in [1_i64, 2, 5] {
        let side = side_degrees * DEGREE;
        let mut worst = f64::INFINITY;
        for _ in 0..30 {
            let area = rolls.box_somewhere(side);
            // Just outside the box, on the side of it, where the longitude floor
            // and the latitude floor are each doing most of the work in turn.
            let from = units(
                (area.west() - side).max(-180 * DEGREE),
                area.south() + side / 2,
            );
            let bound = tessari_geo::no_closer_than(from, area);
            let nearest = probed_distances(from, area)
                .into_iter()
                .fold(f64::INFINITY, f64::min);
            assert!(nearest.is_finite() && nearest > 0.0);
            worst = worst.min(bound / nearest);
        }
        assert!(
            worst > 0.6,
            "at {side_degrees}° the bound fell to {worst} of the true distance, \
             which is too loose to prune with"
        );
    }
}

#[test]
fn the_bound_is_not_the_distance_to_the_middle_of_the_box() {
    // The canonical way to get this wrong. A centroid distance is a distance to
    // one position in the box rather than a floor under all of them, and for a
    // wide box it is larger than the distance to the near edge by thousands of
    // kilometres — so a traversal keyed on it would discard the box holding the
    // nearest record and answer with confidence.
    let area = box_of(0, 0, 60 * DEGREE, 40 * DEGREE);
    let from = units(-DEGREE, 20 * DEGREE);
    let to_the_near_edge = distance(from, units(0, 20 * DEGREE)).expect("not antipodal");
    let to_the_middle = distance(from, units(30 * DEGREE, 20 * DEGREE)).expect("not antipodal");
    let bound = tessari_geo::no_closer_than(from, area);
    assert!(
        to_the_middle > to_the_near_edge * 20.0,
        "the fixture must make the two answers far apart to mean anything"
    );
    assert!(
        bound <= to_the_near_edge + 1e-6,
        "the bound {bound} is above the near edge at {to_the_near_edge}"
    );
}
