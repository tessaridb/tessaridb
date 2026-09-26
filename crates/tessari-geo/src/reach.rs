//! How far a position is from a shape: the distance to its nearest point.
//!
//! # What is measured
//!
//! Zero when the shape covers the position — decided by [`crate::covers`], the
//! exact integer rule every predicate uses, so "covered" and "zero away" can
//! never disagree. Otherwise the geodesic distance, on WGS-84, to the nearest
//! point of the shape's paths and rings, with each edge being the **lon–lat
//! straight line** the geometry says. That is the convention of [`crate::area`]
//! and of every predicate here; measuring to great-circle edges instead would
//! measure to a shape nobody wrote, and a position the predicates call outside
//! could come out zero metres away.
//!
//! # Finding the nearest point of an edge
//!
//! Along an edge, the distance from the position is a function `f(t)` of how far
//! along the edge one is. Its minimum is found by a best-first search over
//! **pieces** of the edges, every piece carrying a floor that nothing on it can
//! be nearer than. Two floors are known and the larger is used:
//!
//! - the **box** floor, [`crate::no_closer_than`] over the piece's bounding box —
//!   a lon–lat straight line stays inside the box of its two ends;
//! - the **reach** floor, `f(middle) − half the piece's length`. A distance
//!   changes by no more than the path walked (the triangle inequality), and the
//!   length of a lon–lat straight line is bounded above by
//!   `√((M_max·Δφ)² + (a·Δλ)²)`: the meridian radius never exceeds its polar
//!   value `a/√(1−e²)`, and no parallel is further from the axis than `a`.
//!
//! A piece whose floor is not below the best distance found so far cannot hold
//! the answer and is dropped. A piece is halved until it is no longer than
//! [`SETTLE_FRACTION`] of the best distance, and is then settled by a
//! golden-section search: on a piece that short relative to its distance, `f` has
//! one minimum. Every figure the search reports is a distance **to an actual
//! point of the shape**, so the answer is never below the truth.
//!
//! # Where it is weakest, stated
//!
//! The settling step assumes the piece bends less than the distance to it does,
//! which holds for every edge a store is written with except very long edges
//! passed far away close to a pole. The floors guarantee that no piece that
//! could hold a nearer point is skipped; the settling is what places the minimum
//! inside the piece that survives.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use tessari_types::Position;

use crate::grid::Snapped;
use crate::measure::{
    SEMI_MAJOR, distance, geodesic, no_closer_than_degrees, squared_eccentricity,
};
use crate::relate::covers;
use crate::shape::Shape;

/// A piece is settled once it is no longer than this share of the best distance.
///
/// Smaller means more halving and fewer pieces settled by the local search;
/// 1/64 keeps the settled piece's bend negligible against its distance while the
/// number of pieces surviving at each halving stays near a dozen.
const SETTLE_FRACTION: f64 = 1.0 / 64.0;

/// How short a settling interval gets before the search stops, in metres.
///
/// A grid unit is about a tenth of a millimetre; locating the nearest point more
/// finely than the positions themselves are held would be precision nobody has.
const SETTLED_METRES: f64 = 1e-4;

/// A bound on golden-section steps, reached only if [`SETTLED_METRES`] is not.
///
/// Each step keeps 0.618 of the interval, so a hundred steps shrink any edge on
/// the planet far below a grid unit; the bound exists so that a non-finite
/// length cannot make the loop endless.
const SETTLE_STEPS: u32 = 100;

/// The distance in metres from `from` to the nearest point of `shape`.
///
/// `Some(0.0)` when the shape covers the position; `Some(+∞)` for an empty shape,
/// which has no point to be near; `None` only when every point the search
/// measured was near-antipodal, where the ellipsoidal solution does not converge.
#[must_use]
pub fn distance_to(from: Snapped, shape: &Shape) -> Option<f64> {
    if covers(shape, &Shape::Point(from)) {
        return Some(0.0);
    }
    let mut best = f64::INFINITY;
    let mut measured = false;
    let mut empty = true;
    shape.each_position(&mut |vertex| {
        empty = false;
        if let Some(metres) = distance(from, vertex) {
            measured = true;
            best = best.min(metres);
        }
    });
    if empty {
        return Some(f64::INFINITY);
    }

    let mut pieces = BinaryHeap::new();
    shape.each_segment(&mut |start, end| {
        let piece = Piece::whole(start.to_position(), end.to_position());
        pieces.push(Waiting {
            floor: piece.box_floor(from),
            piece,
            tightened: false,
        });
    });

    while let Some(next) = pieces.pop() {
        if next.floor >= best {
            break;
        }
        let Waiting {
            piece, tightened, ..
        } = next;
        if !tightened {
            // First time out of the queue: measure the middle, which tightens the
            // floor, and put the piece back on the tighter one.
            let at = geodesic(from.to_position(), piece.at(piece.middle()));
            if let Some(metres) = at {
                measured = true;
                best = best.min(metres);
            }
            let reach = at.map_or(f64::NEG_INFINITY, |metres| {
                metres - piece.length_bound() / 2.0
            });
            pieces.push(Waiting {
                floor: piece.box_floor(from).max(reach),
                piece,
                tightened: true,
            });
            continue;
        }
        if piece.length_bound() <= best * SETTLE_FRACTION || piece.is_indivisible() {
            if let Some(metres) = piece.settle(from) {
                measured = true;
                best = best.min(metres);
            }
            continue;
        }
        for half in piece.halves() {
            pieces.push(Waiting {
                floor: half.box_floor(from),
                piece: half,
                tightened: false,
            });
        }
    }

    measured.then_some(best)
}

/// A stretch of one edge: the parameters `from`..`to` along it.
#[derive(Clone, Copy)]
struct Piece {
    start: Position,
    end: Position,
    from: f64,
    to: f64,
}

impl Piece {
    const fn whole(start: Position, end: Position) -> Self {
        Self {
            start,
            end,
            from: 0.0,
            to: 1.0,
        }
    }

    fn middle(&self) -> f64 {
        self.from + (self.to - self.from) / 2.0
    }

    /// The point `t` of the way along the whole edge.
    fn at(&self, t: f64) -> Position {
        Position::new(
            self.start.longitude + t * (self.end.longitude - self.start.longitude),
            self.start.latitude + t * (self.end.latitude - self.start.latitude),
        )
    }

    fn halves(&self) -> [Self; 2] {
        let middle = self.middle();
        [
            Self {
                to: middle,
                ..*self
            },
            Self {
                from: middle,
                ..*self
            },
        ]
    }

    /// Halving has reached the resolution of the parameter itself.
    fn is_indivisible(&self) -> bool {
        let middle = self.middle();
        middle <= self.from || middle >= self.to
    }

    fn box_floor(&self, from: Snapped) -> f64 {
        let one = self.at(self.from);
        let other = self.at(self.to);
        no_closer_than_degrees(
            from,
            [
                one.longitude.min(other.longitude),
                one.latitude.min(other.latitude),
                one.longitude.max(other.longitude),
                one.latitude.max(other.latitude),
            ],
        )
    }

    /// An upper bound on this piece's length along the ellipsoid, in metres.
    fn length_bound(&self) -> f64 {
        let share = self.to - self.from;
        let across = ((self.end.latitude - self.start.latitude) * share).to_radians();
        let along = ((self.end.longitude - self.start.longitude) * share).to_radians();
        let polar_meridian = SEMI_MAJOR / (1.0 - squared_eccentricity()).sqrt();
        (polar_meridian * across).hypot(SEMI_MAJOR * along)
    }

    /// The least distance found by golden-section search over this piece, ends
    /// included; `None` when every point measured was near-antipodal.
    fn settle(&self, from: Snapped) -> Option<f64> {
        let here = from.to_position();
        let measure = |t: f64| geodesic(here, self.at(t));
        let keep = (5.0_f64.sqrt() - 1.0) / 2.0;
        let mut best: Option<f64> = None;
        let mut take = |value: Option<f64>| {
            if let Some(metres) = value {
                best = Some(best.map_or(metres, |held| held.min(metres)));
            }
            value.unwrap_or(f64::INFINITY)
        };
        take(measure(self.from));
        take(measure(self.to));
        let (mut low, mut high) = (self.from, self.to);
        let mut left = high - keep * (high - low);
        let mut right = low + keep * (high - low);
        let mut at_left = take(measure(left));
        let mut at_right = take(measure(right));
        let per_share = self.length_bound() / (self.to - self.from);
        for _ in 0..SETTLE_STEPS {
            if (high - low) * per_share <= SETTLED_METRES {
                break;
            }
            if at_left <= at_right {
                high = right;
                right = left;
                at_right = at_left;
                left = high - keep * (high - low);
                at_left = take(measure(left));
            } else {
                low = left;
                left = right;
                at_left = at_right;
                right = low + keep * (high - low);
                at_right = take(measure(right));
            }
        }
        best
    }
}

/// A piece in the queue, cheapest floor first.
struct Waiting {
    floor: f64,
    piece: Piece,
    /// Whether the middle has been measured and the floor tightened by it.
    tightened: bool,
}

impl Ord for Waiting {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: the standard heap is a max-heap, and the cheapest floor goes
        // first.
        other.floor.total_cmp(&self.floor)
    }
}

impl PartialOrd for Waiting {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Waiting {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Waiting {}
