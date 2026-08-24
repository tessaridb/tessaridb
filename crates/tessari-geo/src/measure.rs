//! Distance and area, in metres and square metres.
//!
//! # There is no third option, and that is the decision
//!
//! ADR-0026 D2: a **planar** distance over degrees is never offered — not
//! exposed, not labelled, not available behind a flag. A function that returns
//! degrees is a function somebody reads as metres, and the mistake is invisible
//! because the number looks reasonable at every latitude except the ones where
//! it matters. So the only distance this store can compute is the real one, on
//! the ellipsoid.
//!
//! # Distance is Vincenty's inverse solution on WGS-84
//!
//! Sub-millimetre over the whole ellipsoid, and it is the classic formulation
//! rather than an original one, on the same grounds as the rest of this crate:
//! geodesy is decades of subtle work and the wrong place to be inventive.
//!
//! It has one documented weakness — **near-antipodal** pairs, where the
//! iteration oscillates instead of converging. That is answered by refusing:
//! [`distance`] returns `None` rather than the last iterate. The last iterate is
//! a number, it is roughly twenty thousand kilometres, and it is wrong by an
//! amount nobody can bound — which is precisely the shape of answer this crate
//! exists to not produce.
//!
//! # Area is computed on the authalic sphere
//!
//! The sphere with the **same total surface area** as WGS-84, whose radius is
//! derived here from the ellipsoid's own constants rather than quoted.
//!
//! What that means for an answer, stated rather than buried: a region's area is
//! correct in total across the globe and carries a small latitude-dependent
//! error for any single region, because the ellipsoid's surface is redistributed
//! rather than preserved point by point. For the sizes a database is asked about
//! — a district, a delivery zone, a field — it is well under a part in a
//! thousand. A polygon spanning a hemisphere is a different question and this is
//! not the function for it.
//!
//! **Edges are the lon–lat straight lines the geometry actually says**, not
//! great circles. That is the same convention the rest of this crate uses: a
//! stored ring is a sequence of positions joined in the coordinate plane, and
//! computing its area as though the edges were geodesics would report the area
//! of a shape nobody wrote. The consequence is worth knowing: the area of a box
//! from 0°N to 60°N is the area between two **parallels**, which is what a
//! reader of the coordinates expects and is not what a great-circle edge would
//! enclose.

use tessari_types::Position;

use crate::grid::Snapped;
use crate::shape::{Area, Loop, Shape};

/// The semi-major axis of WGS-84, in metres. A defining constant of the datum.
const SEMI_MAJOR: f64 = 6_378_137.0;

/// The flattening of WGS-84, as its defining reciprocal.
const INVERSE_FLATTENING: f64 = 298.257_223_563;

/// How close two successive iterates must be before the solution is accepted.
///
/// In radians of longitude difference, so 10^-12 is well below a micrometre at
/// the equator — two orders finer than the answer is ever reported to.
const CONVERGED: f64 = 1e-12;

/// How many iterations before a pair is declared near-antipodal.
///
/// Vincenty's inverse converges in a handful of steps for everything else, so a
/// run that reaches this bound has not converged slowly, it has failed.
const ATTEMPTS: u32 = 200;

/// The distance along the ellipsoid between two positions, in metres.
///
/// `None` when the two are near-antipodal, where the iteration does not
/// converge. That is a refusal rather than an approximation: the last iterate is
/// a plausible number wrong by an unbounded amount.
#[must_use]
pub fn distance(one: Snapped, other: Snapped) -> Option<f64> {
    let flattening = 1.0 / INVERSE_FLATTENING;
    let semi_minor = SEMI_MAJOR * (1.0 - flattening);

    // The two are put in a fixed order before any arithmetic happens. Vincenty's
    // solution is symmetric on paper and its floating-point evaluation is not:
    // swapping the arguments changes the order of the additions and moves the
    // answer by a couple of units in the last place. That is a fifth of a
    // nanometre and is physically meaningless — but `d(a, b) == d(b, a)` is
    // either an invariant or it is not, and two lines here make it one.
    let (near, far) = if grid_order(one) <= grid_order(other) {
        (one, other)
    } else {
        (other, one)
    };
    let here = near.to_position();
    let there = far.to_position();
    if here == there {
        return Some(0.0);
    }

    let difference = (there.longitude - here.longitude).to_radians();
    let reduced_here = ((1.0 - flattening) * here.latitude.to_radians().tan()).atan();
    let reduced_there = ((1.0 - flattening) * there.latitude.to_radians().tan()).atan();
    let (sin_here, cos_here) = reduced_here.sin_cos();
    let (sin_there, cos_there) = reduced_there.sin_cos();

    let mut lambda = difference;
    let mut settled = None;
    for _ in 0..ATTEMPTS {
        let (sin_lambda, cos_lambda) = lambda.sin_cos();
        let sin_arc = ((cos_there * sin_lambda).powi(2)
            + (cos_here * sin_there - sin_here * cos_there * cos_lambda).powi(2))
        .sqrt();
        if sin_arc == 0.0 {
            // The same point on the ellipsoid, reached by a different route
            // through the arithmetic than the equality test above.
            return Some(0.0);
        }
        let cos_arc = sin_here * sin_there + cos_here * cos_there * cos_lambda;
        let arc = sin_arc.atan2(cos_arc);
        let sin_azimuth = cos_here * cos_there * sin_lambda / sin_arc;
        let cos_squared_azimuth = 1.0 - sin_azimuth * sin_azimuth;
        // Zero on an equatorial line, where the midpoint term is undefined and
        // the series it feeds is zero anyway.
        let cos_twice_midpoint = if cos_squared_azimuth == 0.0 {
            0.0
        } else {
            cos_arc - 2.0 * sin_here * sin_there / cos_squared_azimuth
        };
        let correction = flattening / 16.0
            * cos_squared_azimuth
            * (4.0 + flattening * (4.0 - 3.0 * cos_squared_azimuth));
        let next = difference
            + (1.0 - correction)
                * flattening
                * sin_azimuth
                * (arc
                    + correction
                        * sin_arc
                        * (cos_twice_midpoint
                            + correction
                                * cos_arc
                                * (-1.0 + 2.0 * cos_twice_midpoint * cos_twice_midpoint)));
        if (next - lambda).abs() < CONVERGED {
            settled = Some((
                arc,
                sin_arc,
                cos_arc,
                cos_squared_azimuth,
                cos_twice_midpoint,
            ));
            break;
        }
        lambda = next;
    }

    let (arc, sin_arc, cos_arc, cos_squared_azimuth, cos_twice_midpoint) = settled?;

    let ratio =
        cos_squared_azimuth * (SEMI_MAJOR.powi(2) - semi_minor.powi(2)) / semi_minor.powi(2);
    let series_a =
        1.0 + ratio / 16384.0 * (4096.0 + ratio * (-768.0 + ratio * (320.0 - 175.0 * ratio)));
    let series_b = ratio / 1024.0 * (256.0 + ratio * (-128.0 + ratio * (74.0 - 47.0 * ratio)));
    let shortfall = series_b
        * sin_arc
        * (cos_twice_midpoint
            + series_b / 4.0
                * (cos_arc * (-1.0 + 2.0 * cos_twice_midpoint * cos_twice_midpoint)
                    - series_b / 6.0
                        * cos_twice_midpoint
                        * (-3.0 + 4.0 * sin_arc * sin_arc)
                        * (-3.0 + 4.0 * cos_twice_midpoint * cos_twice_midpoint)));

    Some(semi_minor * series_a * (arc - shortfall))
}

/// The two grid coordinates, in the order that fixes which argument is which.
const fn grid_order(position: Snapped) -> (i64, i64) {
    (position.longitude_units(), position.latitude_units())
}

/// The radius of the sphere with the same surface area as WGS-84, in metres.
///
/// Derived from the datum's own two constants rather than quoted, so it cannot
/// drift from them and there is no third number to keep in step.
#[must_use]
pub fn authalic_radius() -> f64 {
    let flattening = 1.0 / INVERSE_FLATTENING;
    let semi_minor = SEMI_MAJOR * (1.0 - flattening);
    let eccentricity = ((SEMI_MAJOR.powi(2) - semi_minor.powi(2)) / SEMI_MAJOR.powi(2)).sqrt();
    // The closed form for an oblate spheroid's surface area, divided by 4π and
    // square-rooted: the radius a sphere would need to match it.
    let surface = core::f64::consts::TAU
        * SEMI_MAJOR.powi(2)
        * (1.0 + (1.0 - eccentricity.powi(2)) / eccentricity * eccentricity.atanh());
    (surface / (4.0 * core::f64::consts::PI)).sqrt()
}

/// The area a shape encloses, in square metres.
///
/// Zero for anything with no interior — a position, a path, or a collection of
/// them. Holes are subtracted. A collection's area is the sum of its members',
/// which is why the ingest boundary refuses a multi-polygon whose members
/// overlap: it would otherwise be counted twice with nothing to say so.
#[must_use]
pub fn area(shape: &Shape) -> f64 {
    let mut total = 0.0;
    shape.each_area(&mut |part| total += area_of(part));
    total
}

fn area_of(part: &Area) -> f64 {
    let shell = ring_area(&part.shell);
    let holes: f64 = part.holes.iter().map(ring_area).sum();
    (shell - holes).max(0.0)
}

/// The area a single ring encloses, in square metres, always positive.
///
/// The trapezoid sum over the ring's edges, which for edges that are straight in
/// longitude and latitude is exact on a sphere. The sign is the ring's winding
/// and is discarded here: which way round a ring was written is a question for
/// the format, not for how much ground it covers.
fn ring_area(ring: &Loop) -> f64 {
    let radius = authalic_radius();
    let mut doubled = 0.0;
    for edge in ring.windows(2) {
        let (from, to) = (edge[0].to_position(), edge[1].to_position());
        doubled +=
            span(from, to) * (from.latitude.to_radians().sin() + to.latitude.to_radians().sin());
    }
    (radius * radius * doubled / 2.0).abs()
}

/// How far east the edge runs, in radians, taking the short way round.
///
/// An edge is the shorter of the two ways between its ends — the same reading
/// the rest of the store gives a stored shape — so an edge whose endpoints
/// differ by more than half the world is going the other way. Without this, a
/// ring drawn across the antimeridian would report the area of its complement.
fn span(from: Position, to: Position) -> f64 {
    let mut east = (to.longitude - from.longitude).to_radians();
    if east > core::f64::consts::PI {
        east -= core::f64::consts::TAU;
    } else if east < -core::f64::consts::PI {
        east += core::f64::consts::TAU;
    }
    east
}
