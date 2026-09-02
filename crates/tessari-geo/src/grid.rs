//! The precision model, and the reason it is the whole robustness story.
//!
//! # The problem every geometry engine has
//!
//! A geometric predicate asks which side of a line a point falls on. It is
//! computed as the sign of a determinant, and in floating point that determinant
//! is a subtraction of two nearly equal products. When the point is close to the
//! line the true value is tiny and the rounding error is not, so the computed
//! sign can be **wrong**.
//!
//! A wrong sign is not a small error. The algorithms above the predicate —
//! polygon clipping, triangulation, overlay — reason about consistency: if `a`
//! is left of `bc` then `c` is right of `ab`. One inverted sign makes those
//! statements contradict each other, and the algorithm then walks off the end of
//! a ring, loops forever, or emits topology that is not a polygon. The failure
//! surfaces far from its cause, which is why every mature engine carries an
//! adaptive-precision predicate kernel: a fast floating-point filter with an
//! exact fallback built from error-free transformations.
//!
//! # The route taken here instead
//!
//! Adaptive floating-point predicates are decades of subtle work and the wrong
//! place to be original. So this store does not write them. It removes the need
//! for them.
//!
//! Coordinates are **snapped at ingest to a fixed grid** and stored as integers
//! on that grid. A predicate over integers is a determinant over integers, and
//! its sign is **exact by construction** — computed in `i128`, which is wide
//! enough that no product in any predicate this store computes can overflow it.
//! There is no filter, no fallback, and no near-degenerate case that behaves
//! differently from a clear one.
//!
//! # What that is worth here, stated precisely rather than dramatically
//!
//! Degrees are a *small* coordinate range, so it is worth being honest about
//! what this buys. A single orientation test on `f64` degrees has real headroom
//! at this resolution: the products reach about 3 × 10^4 while one grid unit
//! moves the determinant by roughly 10^-7, which is several orders above the
//! rounding error. So the claim is **not** that `f64` would get these particular
//! signs wrong. It is three narrower things:
//!
//! - **No error analysis anywhere.** The sign is right because it is an integer
//!   comparison, not because someone bounded the error once and the bound is
//!   still true after the next change.
//! - **Comparison is transitive.** A float kernel needs a tolerance, and a
//!   tolerance makes equality non-transitive: `a` equals `b`, `b` equals `c`,
//!   `a` differs from `c`. On the grid two positions are the same integer pair
//!   or they are not, so equality composes and no tolerance appears at all.
//! - **Accumulations do not drift.** One determinant is safe in `f64`; a signed
//!   area summed over a hundred thousand coastline vertices is a long chain of
//!   cancelling terms, and that is where float loses digits it never recovers.
//!   The integer sum is exact at any length.
//!
//! The precision model was already mandatory: a store that keeps whatever float
//! arrived produces overlay output that is invalid by its own definition, and a
//! read-modify-write cycle that is not lossless corrupts data slowly. Making the
//! grid integral turns that obligation into the robustness answer as well, which
//! is why this is one decision rather than two — and it is the cheapness of
//! getting both from one choice, not a rescue from imminent float failure, that
//! makes it the right one.
//!
//! # What the grid costs
//!
//! [`SCALE`] is 10^9, so the resolution is 10^-9 degrees — about **0.11 mm** at
//! the equator, and finer as latitude rises. Every real measurement this store
//! will hold is orders of magnitude coarser: a survey-grade GNSS fix is
//! centimetres, a phone is metres. Two positions that differ by less than a
//! tenth of a millimetre become one position, and that is a property to state
//! rather than a loss to hide.
//!
//! The cost that is real: a coordinate carrying more precision than the grid
//! **does not round-trip unchanged**. It round-trips to its snapped value, and
//! then round-trips unchanged forever after. That is what "lossless at the
//! declared precision" means, and it is enforced at the boundary rather than
//! discovered later.

use tessari_types::Position;

/// Grid units per degree: 10^9, giving a resolution of 10^-9 degrees.
///
/// The exponent is chosen so the whole coordinate range stays far inside `i64`
/// while the resolution stays far below anything measurable: 180 × 10^9 is about
/// 1.8 × 10^11, and `i64` holds 9.2 × 10^18.
pub const SCALE: f64 = 1e9;

/// The largest magnitude a longitude can reach on the grid, in grid units.
const LONGITUDE_LIMIT: i64 = 180_000_000_000;

/// The largest magnitude a latitude can reach on the grid, in grid units.
const LATITUDE_LIMIT: i64 = 90_000_000_000;

/// A position snapped to the grid: longitude first, in grid units.
///
/// The type is what makes the rules enforceable. A bare pair of floats carries
/// neither its axis order nor its precision, so both become conventions held in
/// people's heads — and one layer disagreeing puts the data in the wrong
/// hemisphere with no error anywhere. A value of this type is on the grid, in
/// range, longitude first, because there is no way to construct one that is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Snapped {
    longitude: i64,
    latitude: i64,
}

/// Why a position could not be put on the grid.
///
/// `PartialEq` but not `Eq`: the error carries the coordinate that was refused,
/// and one of the reasons it can be refused is that it was a NaN — which does
/// not equal itself. Claiming `Eq` here would be claiming a reflexivity this
/// type does not have.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum OffGrid {
    /// A coordinate was infinite or not a number.
    ///
    /// Refused rather than clamped: there is no position on the sphere that a
    /// NaN means, so choosing one for the caller would invent data.
    #[error("a {axis} of {value} is not a finite coordinate")]
    NotFinite {
        /// Which coordinate.
        axis: Axis,
        /// What arrived.
        value: f64,
    },
    /// A coordinate was finite but off the sphere.
    #[error("a {axis} of {value} degrees is outside {limit}")]
    OutOfRange {
        /// Which coordinate.
        axis: Axis,
        /// What arrived, in degrees.
        value: f64,
        /// The range it had to be in.
        limit: &'static str,
    },
}

/// Which of the two coordinates an error is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Degrees east of the prime meridian.
    Longitude,
    /// Degrees north of the equator.
    Latitude,
}

impl core::fmt::Display for Axis {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Longitude => "longitude",
            Self::Latitude => "latitude",
        })
    }
}

impl Snapped {
    /// Put a position on the grid.
    ///
    /// # Errors
    ///
    /// Returns [`OffGrid`] when either coordinate is not finite or lies off the
    /// sphere. Nothing is clamped and nothing is wrapped: a longitude of 181 is
    /// a mistake somewhere upstream, and silently turning it into -179 would
    /// move a point across the world without saying so.
    pub fn of(position: Position) -> Result<Self, OffGrid> {
        Ok(Self {
            longitude: snap(
                position.longitude,
                Axis::Longitude,
                LONGITUDE_LIMIT,
                "[-180, 180]",
            )?,
            latitude: snap(
                position.latitude,
                Axis::Latitude,
                LATITUDE_LIMIT,
                "[-90, 90]",
            )?,
        })
    }

    /// A position already known to be on the grid, in grid units.
    ///
    /// For decoders and tests. The range is still checked, because a decoder
    /// reports what the bytes said and bytes can be wrong.
    ///
    /// # Errors
    ///
    /// Returns [`OffGrid::OutOfRange`] when either unit count is off the sphere.
    pub fn from_units(longitude: i64, latitude: i64) -> Result<Self, OffGrid> {
        check_range(longitude, Axis::Longitude, LONGITUDE_LIMIT, "[-180, 180]")?;
        check_range(latitude, Axis::Latitude, LATITUDE_LIMIT, "[-90, 90]")?;
        Ok(Self {
            longitude,
            latitude,
        })
    }

    /// Longitude in grid units.
    #[must_use]
    pub const fn longitude_units(self) -> i64 {
        self.longitude
    }

    /// Latitude in grid units.
    #[must_use]
    pub const fn latitude_units(self) -> i64 {
        self.latitude
    }

    /// Whether this position is one of the two poles.
    ///
    /// At latitude ±90 every longitude names the same place, so a longitude
    /// there says nothing about where the position is. That is why an edge
    /// between two positions at one pole is not asked which way round the world
    /// it goes: both readings are the same degenerate point, and there is no
    /// direction to state.
    #[must_use]
    pub const fn is_at_a_pole(self) -> bool {
        self.latitude == LATITUDE_LIMIT || self.latitude.saturating_neg() == LATITUDE_LIMIT
    }

    /// The position this grid point represents, in degrees.
    ///
    /// The inverse of [`Snapped::of`] only for positions that were already on
    /// the grid — which, after ingest, every stored position is.
    #[must_use]
    pub fn to_position(self) -> Position {
        Position::new(
            units_to_degrees(self.longitude),
            units_to_degrees(self.latitude),
        )
    }
}

/// Grid units back to degrees.
///
/// **Invariant the conversion rests on:** `units` is bounded by the grid limit,
/// so `|units| ≤ 1.8 × 10^11`, which is four orders of magnitude below `2^53`.
/// Every grid point is therefore exactly representable as an `f64`, the cast
/// loses nothing, and the single division that follows is correctly rounded —
/// so the result is the nearest double to `units / 10^9` rather than merely a
/// close one.
///
/// That distinction is not cosmetic here. A position compares **bitwise**
/// (see `tessari_types::Position`), so it is not enough for the answer to be
/// within a rounding error of the right degree value: a shape the store hands
/// back must compare equal to the same shape a caller builds from the same grid
/// coordinates. An earlier form of this function split the value and recombined
/// it with a fused multiply-add, which round-tripped correctly but landed one
/// unit in the last place away from the nearest double for about an eighth of
/// all grid points — near enough for arithmetic and not near enough for equality.
#[expect(
    clippy::cast_precision_loss,
    reason = "every grid point is below 2^53, so the cast is exact"
)]
pub(crate) fn units_to_degrees(units: i64) -> f64 {
    units as f64 / SCALE
}

fn snap(value: f64, axis: Axis, limit: i64, spelling: &'static str) -> Result<i64, OffGrid> {
    if !value.is_finite() {
        return Err(OffGrid::NotFinite { axis, value });
    }
    if value.abs() > units_to_degrees(limit) {
        return Err(OffGrid::OutOfRange {
            axis,
            value,
            limit: spelling,
        });
    }
    Ok(degrees_to_units(value))
}

/// Degrees to grid units.
///
/// **Invariant the conversion rests on:** every caller checks `|degrees|`
/// against the grid limit first, so the scaled value is at most 1.8 × 10^11 —
/// four orders of magnitude inside `i64`, and finite because a non-finite value
/// is refused before this is reached. The saturating conversion therefore never
/// saturates, and the check in front of it is what makes that true rather than
/// hoped for.
#[expect(
    clippy::cast_possible_truncation,
    reason = "bounded by the range check every caller performs first"
)]
fn degrees_to_units(degrees: f64) -> i64 {
    (degrees * SCALE).round() as i64
}

fn check_range(units: i64, axis: Axis, limit: i64, spelling: &'static str) -> Result<(), OffGrid> {
    if units.abs() > limit {
        return Err(OffGrid::OutOfRange {
            axis,
            value: units_to_degrees(units),
            limit: spelling,
        });
    }
    Ok(())
}
