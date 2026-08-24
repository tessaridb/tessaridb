//! The box the index actually indexes.
//!
//! # A box is a candidate, never an answer
//!
//! No spatial index indexes geometry. Every one of them indexes a rectangle
//! around it, and the query then re-checks the real shape against the real
//! predicate. That two-stage shape — **filter and refine** — is the architecture
//! of spatial query processing, not an optimisation layered on top of it.
//!
//! Which makes the box's role easy to state and easy to get wrong: a box match
//! is a **candidate**. Returning candidates as results is the failure nobody
//! reports, because a false positive from a bounding box is geographically
//! plausible — a point just outside the park rather than in it, on the right
//! street, in the right city. The store would simply be wrong at a low rate,
//! forever, and every answer would look reasonable.
//!
//! # The box is a write-path invariant
//!
//! It is computed when the geometry is written, in the same batch, and it is
//! never recomputed by a reader or repaired by a background job. A box that can
//! lag its geometry is a box that will, and a query using a stale box misses
//! rows — the one failure direction a filter must not have, since a missing
//! candidate is a silently wrong answer while an extra one is only work.

use crate::grid::Snapped;

/// The smallest axis-aligned rectangle holding a set of positions.
///
/// In grid units, longitude first, like everything else here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bounds {
    west: i64,
    south: i64,
    east: i64,
    north: i64,
}

impl Bounds {
    /// The box around one position — a rectangle of zero extent.
    #[must_use]
    pub fn of_position(position: Snapped) -> Self {
        Self {
            west: position.longitude_units(),
            south: position.latitude_units(),
            east: position.longitude_units(),
            north: position.latitude_units(),
        }
    }

    /// The box around every position in the sequence.
    ///
    /// `None` for an empty sequence: there is no smallest rectangle around
    /// nothing, and returning a degenerate one at the origin would put an empty
    /// geometry off the coast of Africa where it would answer queries.
    #[must_use]
    pub fn of_positions(positions: &[Snapped]) -> Option<Self> {
        let mut held: Option<Self> = None;
        for position in positions {
            held = Some(match held {
                None => Self::of_position(*position),
                Some(bounds) => bounds.widened_to(*position),
            });
        }
        held
    }

    /// This box grown just enough to hold the position.
    #[must_use]
    pub fn widened_to(self, position: Snapped) -> Self {
        Self {
            west: self.west.min(position.longitude_units()),
            south: self.south.min(position.latitude_units()),
            east: self.east.max(position.longitude_units()),
            north: self.north.max(position.latitude_units()),
        }
    }

    /// This box grown just enough to hold the other.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            west: self.west.min(other.west),
            south: self.south.min(other.south),
            east: self.east.max(other.east),
            north: self.north.max(other.north),
        }
    }

    /// Whether the two boxes share any area, edges and corners included.
    ///
    /// Closed rather than open: two boxes touching along an edge do share the
    /// positions on that edge, and a shape lying exactly on a query's border is
    /// a candidate the refine step must be given the chance to judge.
    #[must_use]
    pub const fn meets(self, other: Self) -> bool {
        self.west <= other.east
            && other.west <= self.east
            && self.south <= other.north
            && other.south <= self.north
    }

    /// Whether this box holds the whole of the other.
    #[must_use]
    pub const fn holds(self, other: Self) -> bool {
        self.west <= other.west
            && self.south <= other.south
            && self.east >= other.east
            && self.north >= other.north
    }

    /// Whether the position is inside or on the box.
    #[must_use]
    pub const fn holds_position(self, position: Snapped) -> bool {
        let longitude = position.longitude_units();
        let latitude = position.latitude_units();
        self.west <= longitude
            && longitude <= self.east
            && self.south <= latitude
            && latitude <= self.north
    }

    /// The western edge, in grid units.
    #[must_use]
    pub const fn west(self) -> i64 {
        self.west
    }

    /// The southern edge, in grid units.
    #[must_use]
    pub const fn south(self) -> i64 {
        self.south
    }

    /// The eastern edge, in grid units.
    #[must_use]
    pub const fn east(self) -> i64 {
        self.east
    }

    /// The northern edge, in grid units.
    #[must_use]
    pub const fn north(self) -> i64 {
        self.north
    }

    /// Whether the box spans more than half the world in longitude.
    ///
    /// The symptom of an unsplit antimeridian crossing. A shape from 179°E to
    /// 179°W is a few kilometres wide, but a naive box around its positions runs
    /// the other way round the planet — so it becomes a candidate for every
    /// query in the store, refinement rejects it every single time, and the
    /// index is silently useless for exactly that row.
    ///
    /// The answer is to split the geometry at 180° on ingest. This is what
    /// notices that it was not.
    #[must_use]
    pub const fn spans_more_than_half_the_world(self) -> bool {
        self.east.saturating_sub(self.west) > 180_000_000_000
    }
}
