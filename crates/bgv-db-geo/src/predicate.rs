//! The exact kernel: which side, do they cross, is it inside.
//!
//! Three questions, and every higher geometric operation is built from them. The
//! kernel has no dependencies on the rest of the engine, deliberately — it is
//! the layer whose correctness everything else assumes, so it must be readable
//! and testable on its own.
//!
//! # Every answer here is exact, and that is a property of the grid
//!
//! Positions arrive as integers on a fixed grid ([`crate::grid`]), so every
//! determinant below is a determinant over integers, evaluated in `i128`. The
//! sign is therefore the true sign, always — not usually, not outside a
//! tolerance, and not after a fallback path that is exercised once a year and
//! is wrong when it is.
//!
//! That matters more than it sounds. A predicate that returns the wrong sign
//! near-degenerately does not merely return a slightly wrong answer: it makes
//! the algorithms above it self-contradictory. If `p` is reported left of `ab`
//! while `a` is reported left of `bp`, a clipping routine walks off the end of a
//! ring, a triangulation loops, and an overlay emits something that is not a
//! polygon. The exception then surfaces somewhere else entirely, which is why
//! "topology exception" is the least useful error message in geospatial work.
//!
//! # Why no tolerance appears anywhere
//!
//! A tolerance is what a floating-point kernel needs to paper over its own
//! uncertainty, and it buys the paper by making the predicate **non-transitive**:
//! `a` equals `b` and `b` equals `c` while `a` differs from `c`. On the grid,
//! two positions are either the same integer pair or they are not, so equality
//! is an equivalence relation and comparisons compose.
//!
//! Rounding happens exactly once, at ingest, where it is visible and recorded.

use crate::grid::Snapped;

/// Which side of a directed line a position falls on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Orientation {
    /// Left of the line, travelling from the first position to the second.
    Left,
    /// Right of it.
    Right,
    /// Exactly on it — a real answer here, not a near-miss.
    Collinear,
}

/// Which side of the directed line `from` → `to` the position `of` falls on.
///
/// The sign of the cross product of the two edge vectors, computed in `i128`
/// over grid units. No product can overflow: a coordinate difference is at most
/// 3.6 × 10^11 and a longitude is at most 1.8 × 10^11, so the largest term is
/// about 6.5 × 10^22 against `i128`'s 1.7 × 10^38.
#[must_use]
pub fn orientation(from: Snapped, to: Snapped, of: Snapped) -> Orientation {
    let edge_longitude = difference(to.longitude_units(), from.longitude_units());
    let edge_latitude = difference(to.latitude_units(), from.latitude_units());
    let to_longitude = difference(of.longitude_units(), from.longitude_units());
    let to_latitude = difference(of.latitude_units(), from.latitude_units());

    // Saturating rather than bare, because the workspace refuses arithmetic that
    // could wrap. Neither product can reach the saturation point: the bound above
    // puts the largest at about 6.5 × 10^22 against `i128`'s 1.7 × 10^38, so
    // these are exact products written in a form that says so.
    match edge_longitude
        .saturating_mul(to_latitude)
        .cmp(&edge_latitude.saturating_mul(to_longitude))
    {
        core::cmp::Ordering::Greater => Orientation::Left,
        core::cmp::Ordering::Less => Orientation::Right,
        core::cmp::Ordering::Equal => Orientation::Collinear,
    }
}

/// One coordinate minus another, widened first so the subtraction cannot wrap.
///
/// Both operands fit in `i64`, so their difference fits in `i128` with 17 orders
/// of magnitude to spare; the saturating form is the workspace's spelling for
/// "this cannot overflow", not a claim that it might.
fn difference(left: i64, right: i64) -> i128 {
    i128::from(left).saturating_sub(i128::from(right))
}

/// Whether `of` lies on the closed segment `from` → `to`, endpoints included.
///
/// Collinearity alone is not enough: every position on the infinite line through
/// the segment is collinear with it, including the ones far outside. The second
/// test is the segment's bounding box, which for a collinear position is exactly
/// the condition of lying between the ends.
#[must_use]
pub fn on_segment(from: Snapped, to: Snapped, of: Snapped) -> bool {
    orientation(from, to, of) == Orientation::Collinear
        && between(
            of.longitude_units(),
            from.longitude_units(),
            to.longitude_units(),
        )
        && between(
            of.latitude_units(),
            from.latitude_units(),
            to.latitude_units(),
        )
}

fn between(value: i64, one_end: i64, other_end: i64) -> bool {
    value >= one_end.min(other_end) && value <= one_end.max(other_end)
}

/// Whether two closed segments share at least one position.
///
/// Touching counts and overlapping counts, because a shared endpoint and a
/// shared stretch are both "they meet" — and a predicate that answered otherwise
/// would report two edges of the same ring as disjoint.
#[must_use]
pub fn segments_meet(
    first_from: Snapped,
    first_to: Snapped,
    second_from: Snapped,
    second_to: Snapped,
) -> bool {
    let a = orientation(first_from, first_to, second_from);
    let b = orientation(first_from, first_to, second_to);
    let c = orientation(second_from, second_to, first_from);
    let d = orientation(second_from, second_to, first_to);

    // The ordinary case: each segment straddles the other's line.
    if a != b && c != d && a != Orientation::Collinear && c != Orientation::Collinear {
        return true;
    }

    // The collinear and touching cases, which the straddle test above cannot see
    // and which are the ones real data is full of.
    on_segment(first_from, first_to, second_from)
        || on_segment(first_from, first_to, second_to)
        || on_segment(second_from, second_to, first_from)
        || on_segment(second_from, second_to, first_to)
}

/// Where a position sits with respect to a ring.
///
/// Three answers rather than two, because the boundary is a real place and the
/// DE-9IM predicates disagree about it on purpose: `contains` excludes a
/// boundary position where `covers` includes it. A predicate layer that folded
/// the boundary into one of the other two answers could not express both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Containment {
    /// Strictly inside.
    Inside,
    /// Exactly on an edge or a vertex.
    Boundary,
    /// Strictly outside.
    Outside,
}

/// Where `of` sits with respect to the closed ring `ring`.
///
/// The ring is a sequence of positions whose first and last are the same. The
/// boundary is tested first and separately, because the winding count below is
/// undefined on it — a position on an edge is neither wound around nor not.
///
/// Then the winding number, in Sunday's formulation: count the edges crossing
/// the horizontal line through `of`, signed by direction, and count only those
/// crossing strictly to one side. A non-zero winding is inside.
///
/// Winding rather than crossing parity, deliberately: they agree for a simple
/// ring and disagree for a self-overlapping one, and winding gives the answer a
/// person means — a region covered twice is still inside.
#[must_use]
pub fn ring_contains(ring: &[Snapped], of: Snapped) -> Containment {
    if ring.len() < 4 {
        // Fewer than four positions cannot close around any area, so there is no
        // inside to be in. Reported as outside rather than refused: the caller
        // that cares about well-formedness asked at ingest.
        return Containment::Outside;
    }
    for edge in ring.windows(2) {
        let (from, to) = (edge[0], edge[1]);
        if on_segment(from, to, of) {
            return Containment::Boundary;
        }
    }

    let mut winding = 0_i32;
    for edge in ring.windows(2) {
        let (from, to) = (edge[0], edge[1]);
        let latitude = of.latitude_units();
        if from.latitude_units() <= latitude {
            if to.latitude_units() > latitude && orientation(from, to, of) == Orientation::Left {
                winding = winding.saturating_add(1);
            }
        } else if to.latitude_units() <= latitude && orientation(from, to, of) == Orientation::Right
        {
            winding = winding.saturating_sub(1);
        }
    }

    if winding == 0 {
        Containment::Outside
    } else {
        Containment::Inside
    }
}

/// Twice the signed area of a ring, in squared grid units.
///
/// Doubled so it stays an integer, and signed so its sign is the ring's
/// orientation: positive counter-clockwise, negative clockwise. RFC 7946 asks an
/// exterior ring to be counter-clockwise and a hole to be clockwise, and this is
/// the function that decides whether a ring obeys.
///
/// This is a **planar** area over grid units and is not a measurement. It exists
/// to compare against zero, not to be reported to anybody — a real area on the
/// sphere is a different computation with a different unit.
#[must_use]
pub fn twice_signed_area(ring: &[Snapped]) -> i128 {
    // `sum()` over `i128` would be bare arithmetic; folding with a saturating
    // add keeps the same total. A ring of a hundred thousand vertices contributes
    // terms of at most 3.3 × 10^22 each, so even a pathological ring stays some
    // twelve orders below saturation.
    ring.windows(2)
        .map(|edge| {
            let (from, to) = (edge[0], edge[1]);
            i128::from(from.longitude_units())
                .saturating_mul(i128::from(to.latitude_units()))
                .saturating_sub(
                    i128::from(to.longitude_units())
                        .saturating_mul(i128::from(from.latitude_units())),
                )
        })
        .fold(0_i128, i128::saturating_add)
}
