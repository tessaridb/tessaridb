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

    /// A box stated by its four edges, if they are a box.
    ///
    /// The way a stored box is read back, and the only constructor that can be
    /// handed edges the geometry never produced. `None` when an edge runs
    /// backwards — every box this crate builds has `west ≤ east` and
    /// `south ≤ north`, so a pair that does not is bytes that did not come from
    /// one, and admitting it would create a rectangle holding no position while
    /// still answering `meets` for a strip of the world.
    #[must_use]
    pub const fn of_corners(west: i64, south: i64, east: i64, north: i64) -> Option<Self> {
        if west > east || south > north {
            return None;
        }
        Some(Self {
            west,
            south,
            east,
            north,
        })
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

    /// Whether the two boxes are the same rectangle.
    ///
    /// Named rather than left to `==`, so that [`Relation`] reads as four
    /// statements of the same kind and a reader comparing them is comparing
    /// like with like.
    #[must_use]
    pub const fn same_as(self, other: Self) -> bool {
        self.west == other.west
            && self.south == other.south
            && self.east == other.east
            && self.north == other.north
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

/// Which box test a spatial filter is allowed to use.
///
/// # One test per predicate, and never one test for all of them
///
/// The filter step is only correct when it admits a **superset** of what the
/// exact predicate would. A test narrower than its predicate drops true results
/// silently; a test wider than it needs to be only costs refinement. So the two
/// mistakes are not symmetric, and the shape of this type follows from that: a
/// predicate names its relation, and a predicate with no sound box test — the
/// complement of one, such as *disjoint* — has no variant here at all and takes
/// the scan instead.
///
/// The relation is stated from the **query's** point of view, because that is
/// the argument a reader wrote and the one a plan is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Relation {
    /// The record shares a position with the query.
    ///
    /// Superset of `intersects`: two shapes sharing a position share it inside
    /// both their boxes.
    Meets,
    /// The record lies inside the query.
    ///
    /// Superset of `within` and `covered_by`: every position of a shape inside
    /// the query is inside the query's box, so the record's box is too.
    Inside,
    /// The record holds the whole query.
    ///
    /// Superset of `contains` and `covers` — the converse of [`Self::Inside`],
    /// and the one that reaches records **larger** than the query, which is why
    /// a lookup cannot be only a forward scan.
    Around,
    /// The record occupies exactly the query's ground.
    ///
    /// Superset of `equals`: equal shapes have equal boxes.
    Same,
}

impl Relation {
    /// Whether a record with this box may still satisfy the predicate.
    ///
    /// Never the other way round: a `true` here says only that the exact
    /// predicate has to be run, and a `false` says it cannot possibly hold.
    #[must_use]
    pub const fn admits(self, query: Bounds, record: Bounds) -> bool {
        match self {
            Self::Meets => query.meets(record),
            Self::Inside => query.holds(record),
            Self::Around => record.holds(query),
            Self::Same => query.same_as(record),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bounds, Relation};
    use crate::grid::Snapped;

    fn at(longitude: i64, latitude: i64) -> Snapped {
        Snapped::from_units(longitude, latitude).expect("a position on the grid")
    }

    fn box_of(west: i64, south: i64, east: i64, north: i64) -> Bounds {
        Bounds::of_position(at(west, south)).widened_to(at(east, north))
    }

    #[test]
    fn each_relation_admits_the_case_it_is_for_and_refuses_its_opposite() {
        let query = box_of(0, 0, 100, 100);
        let inside = box_of(10, 10, 20, 20);
        let around = box_of(-50, -50, 200, 200);
        let overlapping = box_of(50, 50, 200, 200);
        let away = box_of(500, 500, 600, 600);

        assert!(Relation::Meets.admits(query, inside));
        assert!(Relation::Meets.admits(query, around));
        assert!(Relation::Meets.admits(query, overlapping));
        assert!(!Relation::Meets.admits(query, away));

        assert!(Relation::Inside.admits(query, inside));
        assert!(!Relation::Inside.admits(query, around));
        assert!(!Relation::Inside.admits(query, away));

        assert!(Relation::Around.admits(query, around));
        assert!(!Relation::Around.admits(query, inside));
        assert!(!Relation::Around.admits(query, away));

        assert!(Relation::Same.admits(query, query));
        assert!(!Relation::Same.admits(query, inside));
        assert!(!Relation::Same.admits(query, around));
    }

    #[test]
    fn a_box_touching_along_an_edge_meets_it() {
        // The boundary case, and the reason `meets` is closed: a shape lying
        // exactly on the query's border is a candidate the exact predicate must
        // be given the chance to judge. Opening this comparison would drop every
        // `touches` answer before the predicate ever saw it.
        let query = box_of(0, 0, 100, 100);
        assert!(Relation::Meets.admits(query, box_of(100, 0, 200, 100)));
        assert!(Relation::Meets.admits(query, box_of(100, 100, 200, 200)));
        assert!(!Relation::Meets.admits(query, box_of(101, 0, 200, 100)));
    }

    #[test]
    fn every_relation_admits_a_box_equal_to_the_query() {
        // The one record every relation has to keep: a shape identical to the
        // query satisfies `intersects`, `within`, `contains` and `equals` all at
        // once, so a relation refusing it is narrower than its own predicate and
        // would drop a true result.
        let query = box_of(-10, -10, 10, 10);
        for relation in [
            Relation::Meets,
            Relation::Inside,
            Relation::Around,
            Relation::Same,
        ] {
            assert!(
                relation.admits(query, query),
                "{relation:?} should admit a record occupying the query's own ground"
            );
        }
    }
}
