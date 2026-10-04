//! Where a distance from a position may not converge: around its antipode
//! (G058 C1).
//!
//! [`crate::distance`] answers `None` for a near-antipodal pair, and the value
//! layer turns that into `NONE`, which sorts below every number — so a read
//! ordered by distance puts such a record **first**, while a walk that ranks by
//! floors would reach it last, its floor being half the planet. A walk asks
//! these boxes first and gives the read to the scan if the index holds anything
//! in them.
//!
//! The zone was measured: over seven latitudes and four longitudes, every
//! position within three degrees of the antipode in each axis, at a hundredth of
//! a degree, `None` came back no further than 0.67° in latitude or longitude.
//! [`ANTIPODAL_MARGIN_DEGREES`] is three times that.

use crate::bounds::Bounds;
use crate::grid::{Snapped, degrees_to_units};

/// How far from the antipode, in degrees along each axis, a distance may fail
/// to converge — the measured 0.67°, tripled.
pub const ANTIPODAL_MARGIN_DEGREES: f64 = 2.0;

/// The boxes around `target`'s antipode where a distance from it may not
/// converge: one, or two where the zone crosses the antimeridian.
#[must_use]
pub fn antipodal_zone(target: Snapped) -> Vec<Bounds> {
    let here = target.to_position();
    let longitude = if here.longitude > 0.0 {
        here.longitude - 180.0
    } else {
        here.longitude + 180.0
    };
    let latitude = -here.latitude;
    // Every edge below is clamped to the grid's range first, which is the
    // invariant the conversion rests on.
    let units = degrees_to_units;
    let south = units((latitude - ANTIPODAL_MARGIN_DEGREES).max(-90.0));
    let north = units((latitude + ANTIPODAL_MARGIN_DEGREES).min(90.0));
    let west = longitude - ANTIPODAL_MARGIN_DEGREES;
    let east = longitude + ANTIPODAL_MARGIN_DEGREES;
    let mut zone = Vec::with_capacity(2);
    let mut push = |from: f64, to: f64| {
        if let Some(bounds) = Bounds::of_corners(units(from), south, units(to), north) {
            zone.push(bounds);
        }
    };
    if west < -180.0 {
        push(-180.0, east);
        push(west + 360.0, 180.0);
    } else if east > 180.0 {
        push(west, 180.0);
        push(-180.0, east - 360.0);
    } else {
        push(west, east);
    }
    zone
}
