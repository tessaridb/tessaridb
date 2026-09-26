//! The box holding everything within a distance of a box.
//!
//! # What it is for
//!
//! A radius read — every record within `r` metres of a place — is a filter a
//! spatial index can narrow, but only through a **box**: the index keys records
//! by the cells of their own boxes. So the question is turned into "which
//! records' boxes meet the query's box widened by `r`", and the exact distance is
//! then tested on those candidates. The widening must hold **every** position
//! within `r` of the query, or the read answers with fewer rows than exist and
//! raises nothing — the one failure direction a filter must not have.
//!
//! # Why the widened box certainly holds them
//!
//! The two floors under [`crate::no_closer_than`] say how far a position can be
//! from another, and each is inverted here:
//!
//! - **Latitude.** A path between two positions crosses every parallel between
//!   them and is at least `M_min · Δφ` long, `M_min` being the meridian radius at
//!   the equator, the smallest anywhere. So a position within `r` is within
//!   `r / M_min` radians of latitude.
//! - **Longitude.** A position at a turn `Δλ` from `q` is at least
//!   `ρ(q) · sin Δλ` away while `Δλ` is under a right angle, `ρ(q)` being `q`'s
//!   distance from the spin axis — the chord to the meridian half-plane. The
//!   query's positions are no nearer the axis than its box's most poleward
//!   parallel allows, so with `ρ_min` taken there, a position within `r` turns by
//!   at most `asin(r / ρ_min)`. When `r` reaches `ρ_min` the turn is not bounded
//!   at all, and the box spans every longitude.
//!
//! A widening that reaches a pole also spans every longitude, since every
//! meridian meets there. So does one that would cross ±180: the store's boxes do
//! not wrap, and the whole band is a superset of the two pieces it would take.
//!
//! The edges are rounded **outward** by a grid unit beyond the rounding itself,
//! and the distance is inflated by a part in a billion before any of it, so
//! floating-point error in the inversion can only widen the box.

use crate::bounds::Bounds;
use crate::grid::{Snapped, units_to_degrees};
use crate::measure::{distance_from_axis, least_meridian_radius};
use tessari_types::Position;

/// The box holding every position within `metres` of a position in `bounds`.
///
/// `None` when `metres` is negative or not finite: no position is a negative
/// distance away, and an unbounded distance holds the whole world, so neither is
/// a question a box answers better than reading everything does.
#[must_use]
pub fn within_reach(bounds: Bounds, metres: f64) -> Option<Bounds> {
    if !metres.is_finite() || metres < 0.0 {
        return None;
    }
    let metres = metres * (1.0 + 1e-9) + 1e-6;
    let south = units_to_degrees(bounds.south());
    let north = units_to_degrees(bounds.north());
    let turn = (metres / least_meridian_radius()).to_degrees();
    let (south, north) = (south - turn, north + turn);
    let reaches_a_pole = south <= -90.0 || north >= 90.0;

    let poleward = units_to_degrees(bounds.south())
        .abs()
        .max(units_to_degrees(bounds.north()).abs());
    let nearest_axis = distance_from_axis(poleward);
    let (west, east) = if reaches_a_pole || metres >= nearest_axis {
        (-180.0, 180.0)
    } else {
        let sweep = (metres / nearest_axis).asin().to_degrees();
        let west = units_to_degrees(bounds.west()) - sweep;
        let east = units_to_degrees(bounds.east()) + sweep;
        if west < -180.0 || east > 180.0 {
            (-180.0, 180.0)
        } else {
            (west, east)
        }
    };

    let low = Snapped::of(Position::new(west, south.max(-90.0))).ok()?;
    let high = Snapped::of(Position::new(east, north.min(90.0))).ok()?;
    let widened = Bounds::of_position(low).widened_to(high);
    let limit = Snapped::of(Position::new(180.0, 90.0)).ok()?;
    Bounds::of_corners(
        widened
            .west()
            .saturating_sub(1)
            .max(limit.longitude_units().saturating_neg()),
        widened
            .south()
            .saturating_sub(1)
            .max(limit.latitude_units().saturating_neg()),
        widened
            .east()
            .saturating_add(1)
            .min(limit.longitude_units()),
        widened
            .north()
            .saturating_add(1)
            .min(limit.latitude_units()),
    )
}
