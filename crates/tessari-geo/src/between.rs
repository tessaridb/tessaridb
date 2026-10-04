//! How far apart two shapes are: the least distance between any point of one
//! and any point of the other (G058 C1).
//!
//! # What is measured
//!
//! Zero when the shapes share a point — decided by [`crate::intersects`], the
//! exact integer rule every predicate uses. Otherwise the least, over the points
//! of `one`, of [`distance_to`] the other: the distance from a point to a shape is
//! already the nearest point's, so the only search left is over `one`.
//!
//! Over `one`, only its paths and rings need searching. Two shapes that share no
//! point have their nearest pair on the boundary of each: from a point inside an
//! area, the geodesic to the other shape leaves the area first, and the point
//! where it leaves is nearer.
//!
//! # The search
//!
//! The one [`distance_to`] runs, one level up. Best-first over pieces of `one`'s
//! edges: a distance to a fixed shape moves by no more than the path walked, so
//! `distance_to(middle) − half the piece's length` is a floor for every point of
//! the piece, and every figure measured is a distance to an actual point of each
//! shape — the answer is never below the truth. A piece no longer than
//! [`crate::reach`]'s settle fraction of the best distance is settled by a
//! golden-section search, under the assumption that module states: on a piece
//! that short relative to its distance the distance has one minimum.
//!
//! A point measured from is first put on the grid, which moves it by less than a
//! tenth of a millimetre — far inside the settling resolution.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::grid::Snapped;
use crate::reach::{Piece, SETTLE_FRACTION, SETTLE_STEPS, SETTLED_METRES, distance_to};
use crate::relate::intersects;
use crate::shape::Shape;

/// How far above the true least distance an answer may be, in metres, where the
/// settling assumption holds: the golden-section search stops at
/// [`crate::reach`]'s settled resolution, and the grid moves a point by less than
/// a tenth of a millimetre.
pub const TOLERANCE_METRES: f64 = 1e-3;

/// The least distance in metres between a point of `one` and a point of
/// `other`, to within [`TOLERANCE_METRES`] above it.
///
/// `Some(0.0)` when they share a point; `Some(+∞)` when either is empty;
/// `None` only when every measurement was near-antipodal, where the ellipsoidal
/// solution does not converge.
#[must_use]
pub fn distance_between(one: &Shape, other: &Shape) -> Option<f64> {
    if is_empty(one) || is_empty(other) {
        return Some(f64::INFINITY);
    }
    if intersects(one, other) {
        return Some(0.0);
    }
    // Search the shape with fewer edges; measure from it to the other.
    let (from, to) = if edges(one) <= edges(other) {
        (one, other)
    } else {
        (other, one)
    };
    let mut best = f64::INFINITY;
    let mut measured = false;
    from.each_position(&mut |vertex| {
        if let Some(metres) = distance_to(vertex, to) {
            measured = true;
            best = best.min(metres);
        }
    });

    let measure = |piece: &Piece, t: f64| {
        Snapped::of(piece.at(t))
            .ok()
            .and_then(|point| distance_to(point, to))
    };
    let mut pieces = BinaryHeap::new();
    from.each_segment(&mut |start, end| {
        let piece = Piece::whole(start.to_position(), end.to_position());
        pieces.push(Waiting {
            floor: f64::NEG_INFINITY,
            piece,
        });
    });
    while let Some(Waiting { floor, piece }) = pieces.pop() {
        if floor >= best {
            break;
        }
        if piece.length_bound() <= best * SETTLE_FRACTION || piece.is_indivisible() {
            if let Some(metres) = settle(&piece, &measure) {
                measured = true;
                best = best.min(metres);
            }
            continue;
        }
        for half in piece.halves() {
            let Some(metres) = measure(&half, half.middle()) else {
                continue;
            };
            measured = true;
            best = best.min(metres);
            pieces.push(Waiting {
                floor: metres - half.length_bound() / 2.0,
                piece: half,
            });
        }
    }
    measured.then_some(best)
}

/// The least distance found by golden-section search over `piece`, ends
/// included; `None` when nothing measured converged.
fn settle(piece: &Piece, measure: &impl Fn(&Piece, f64) -> Option<f64>) -> Option<f64> {
    let keep = (5.0_f64.sqrt() - 1.0) / 2.0;
    let (start, finish) = piece.span();
    let mut best: Option<f64> = None;
    let mut take = |value: Option<f64>| {
        if let Some(metres) = value {
            best = Some(best.map_or(metres, |held| held.min(metres)));
        }
        value.unwrap_or(f64::INFINITY)
    };
    take(measure(piece, start));
    take(measure(piece, finish));
    let (mut low, mut high) = (start, finish);
    let mut left = high - keep * (high - low);
    let mut right = low + keep * (high - low);
    let mut at_left = take(measure(piece, left));
    let mut at_right = take(measure(piece, right));
    let per_share = piece.length_bound() / (finish - start);
    for _ in 0..SETTLE_STEPS {
        if (high - low) * per_share <= SETTLED_METRES {
            break;
        }
        if at_left <= at_right {
            high = right;
            right = left;
            at_right = at_left;
            left = high - keep * (high - low);
            at_left = take(measure(piece, left));
        } else {
            low = left;
            left = right;
            at_left = at_right;
            right = low + keep * (high - low);
            at_right = take(measure(piece, right));
        }
    }
    best
}

/// Whether a shape holds no position at all.
fn is_empty(shape: &Shape) -> bool {
    let mut empty = true;
    shape.each_position(&mut |_| empty = false);
    empty
}

/// How many edges a shape has — which side the search runs over.
fn edges(shape: &Shape) -> usize {
    let mut count = 0_usize;
    shape.each_segment(&mut |_, _| count = count.saturating_add(1));
    count
}

/// A piece in the queue, cheapest floor first.
struct Waiting {
    floor: f64,
    piece: Piece,
}

impl Ord for Waiting {
    fn cmp(&self, other: &Self) -> Ordering {
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
