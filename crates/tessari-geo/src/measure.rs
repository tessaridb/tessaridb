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

use crate::bounds::Bounds;
use crate::grid::{Snapped, units_to_degrees};
use crate::shape::{Area, Loop, Shape};

/// The semi-major axis of WGS-84, in metres. A defining constant of the datum.
pub(crate) const SEMI_MAJOR: f64 = 6_378_137.0;

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
    geodesic(near.to_position(), far.to_position())
}

/// Vincenty's inverse solution between two positions that need not be on the
/// grid, in metres; `None` when near-antipodal.
///
/// [`distance`] is this with its arguments put in a fixed order first. The
/// search for the nearest point of an edge ([`crate::reach`]) evaluates points
/// *between* grid positions, and snapping each of them would move the point it
/// measures to by up to half a grid unit, so it calls this directly.
pub(crate) fn geodesic(here: Position, there: Position) -> Option<f64> {
    let flattening = 1.0 / INVERSE_FLATTENING;
    let semi_minor = SEMI_MAJOR * (1.0 - flattening);
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

/// A distance from `from` that nothing inside `bounds` can be nearer than, in
/// metres. Zero when `from` is inside the box.
///
/// # What this is for, and the mistake it exists to prevent
///
/// A nearest-first traversal orders unexplored regions by how close they could
/// possibly be, and stops when the best region left is further away than the
/// worst answer already held. The whole of that argument rests on *could
/// possibly be*: if the key were ever **larger** than the true distance to
/// something inside the box, the walk would discard a region holding a nearer
/// record and answer confidently with the wrong one.
///
/// The canonical way to get this wrong is to measure to the box's centre, which
/// is a distance to one position in the box rather than a floor under all of
/// them. A centroid key produces an answer that is plausible, ordered, and wrong
/// — and a test written from the same idea agrees with it.
///
/// # Why the number returned is certainly a floor
///
/// Two independent floors are computed and the larger is taken, which is a floor
/// because the maximum of two floors is one.
///
/// **Across latitude.** Every position in the box has latitude in
/// `[south, north]`. If `from` is south of that band, a path to any of them is
/// continuous in latitude and therefore crosses the parallel `south`, so it is
/// at least as long as the distance from `from` to that parallel. That distance
/// is the meridian arc — the distance to a parallel is rotationally invariant
/// and a meridian is a geodesic meeting it at a right angle — and a meridian arc
/// over `Δφ` is at least `M_min · Δφ`, since the meridian radius of curvature is
/// smallest at the equator and larger everywhere else.
///
/// **Across longitude.** Every position in the box has longitude in
/// `[west, east]`, so all of them lie in the wedge swept by the meridian
/// half-planes over that span — a wedge that contains the spin axis. The
/// straight-line distance in space from `from` to that wedge is `ρ·sin(Δλ)`,
/// where `ρ` is `from`'s own distance from the axis, falling back to `ρ` once the
/// turn passes a right angle and the nearest point of the wedge becomes the axis
/// itself. A geodesic is a curve joining two points, so it is never shorter than
/// the straight line between them: a chord is a floor under a surface distance,
/// exactly and with nothing approximated.
///
/// # What it costs
///
/// Taking the larger of the two rather than combining them leaves the bound
/// loose by at most a factor of `√2`, on an approach diagonal to both edges.
/// That is paid in cells a traversal opens and never in which records it
/// answers with.
///
/// Unlike [`distance`], this **cannot fail**. It is closed form, so there is no
/// iteration to not converge — which matters, because a bound with no answer has
/// no safe default: zero would make a traversal read everything, and any other
/// guess would make it wrong.
/// A position the box **holds** needs no case of its own: both floors are zero
/// exactly when the position is inside their own axis's span, so the answer for
/// a position inside the box falls out of the two of them. An early return here
/// would be a third statement of the same rule and a fourth place for it to
/// disagree with itself — and, being unreachable, one no test could hold to
/// account.
#[must_use]
pub fn no_closer_than(from: Snapped, bounds: Bounds) -> f64 {
    no_closer_than_degrees(
        from,
        [
            units_to_degrees(bounds.west()),
            units_to_degrees(bounds.south()),
            units_to_degrees(bounds.east()),
            units_to_degrees(bounds.north()),
        ],
    )
}

/// [`no_closer_than`] for a box given in degrees, `[west, south, east, north]`.
///
/// The same two floors; the box need not lie on the grid, which is what a piece
/// of an edge between two grid positions needs.
pub(crate) fn no_closer_than_degrees(from: Snapped, bounds: [f64; 4]) -> f64 {
    let [west, south, east, north] = bounds;
    across_latitude(from, south, north).max(across_longitude(from, west, east))
}

/// The floor that the band of latitudes alone puts under the distance.
fn across_latitude(from: Snapped, south: f64, north: f64) -> f64 {
    let latitude = units_to_degrees(from.latitude_units());
    let turn = if latitude < south {
        south - latitude
    } else if latitude > north {
        latitude - north
    } else {
        return 0.0;
    };
    least_meridian_radius() * turn.to_radians()
}

/// The floor that the span of longitudes alone puts under the distance.
fn across_longitude(from: Snapped, west: f64, east: f64) -> f64 {
    let longitude = units_to_degrees(from.longitude_units());
    if west <= longitude && longitude <= east {
        return 0.0;
    }
    // The span is an interval, and the turn to a point inside it is largest at
    // the antipode rather than smallest, so the nearest longitude in the span is
    // one of its two ends whenever `from` is outside.
    let turn = shortest_turn(longitude, west).min(shortest_turn(longitude, east));
    let axis = distance_from_axis(units_to_degrees(from.latitude_units()));
    if turn >= 90.0 {
        axis
    } else {
        axis * turn.to_radians().sin()
    }
}

/// The angle between two longitudes, taken the short way round, in degrees.
fn shortest_turn(one: f64, other: f64) -> f64 {
    let turn = (one - other).abs() % 360.0;
    if turn > 180.0 { 360.0 - turn } else { turn }
}

/// How far a position on the ellipsoid stands from the spin axis, in metres.
pub(crate) fn distance_from_axis(latitude: f64) -> f64 {
    let (sin, cos) = latitude.to_radians().sin_cos();
    let prime_vertical = SEMI_MAJOR / (1.0 - squared_eccentricity() * sin * sin).sqrt();
    prime_vertical * cos
}

/// The smallest meridian radius of curvature on WGS-84, in metres.
///
/// `a(1 − e²)`, which the meridian radius takes at the equator and exceeds at
/// every other latitude — so it is the multiplier that turns a difference in
/// latitude into a distance that is certainly not an overestimate.
pub(crate) fn least_meridian_radius() -> f64 {
    SEMI_MAJOR * (1.0 - squared_eccentricity())
}

/// The square of WGS-84's first eccentricity, from the datum's two defining
/// constants.
pub(crate) fn squared_eccentricity() -> f64 {
    let flattening = 1.0 / INVERSE_FLATTENING;
    flattening * (2.0 - flattening)
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
