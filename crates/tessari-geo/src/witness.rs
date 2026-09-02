//! A position strictly inside a ring, constructed rather than searched for.
//!
//! # Why anything needs this
//!
//! Deciding whether one shape covers another eventually asks whether a **hole**
//! of the covering shape reaches into the covered one. A hole's vertices are no
//! use for that question: they lie on the covering shape's own boundary, so they
//! are inside it, and the part that is *not* inside it is the hole's open
//! interior — which contains no vertex at all.
//!
//! So the interior has to be witnessed by a position that is not a vertex. And
//! it has to be an **exact** position, because a probe point that has been
//! rounded is a probe point that answers about somewhere else.
//!
//! # How, and why the answer is exact
//!
//! The classic ear construction, which needs nothing but the comparisons this
//! crate already has:
//!
//! 1. Take the vertex `b` with the lowest latitude, breaking ties on longitude.
//!    That vertex is necessarily **convex** — nothing can lie below the bottom.
//! 2. Look at the triangle made by `b` and its two neighbours `a` and `c`.
//! 3. If no other vertex of the ring lies strictly inside that triangle, the
//!    triangle is an ear and the midpoint of `a`→`c` is inside the ring.
//! 4. Otherwise take the intruding vertex `q` farthest from the line `a`→`c`;
//!    the segment `b`→`q` lies inside the ring, so its midpoint does too.
//!
//! Every midpoint here is the midpoint of two **grid** vertices, which is why
//! the answer is a [`Fine`] — a half-unit position, exact by construction. No
//! step takes a midpoint of a midpoint, which is the one thing that arithmetic
//! would not survive.

use crate::grid::Snapped;
use crate::predicate::{Fine, Orientation, orientation};

/// A position strictly inside the ring, or `None` when the ring bounds nothing.
///
/// `None` for a ring too short to enclose area or one whose vertices are all
/// collinear. Both are refused at ingest, so a stored ring always has a witness;
/// a ring that arrived as a query argument may not.
pub(crate) fn inside_ring(ring: &[Snapped]) -> Option<Fine> {
    // The closing repeat is not a distinct corner, and the neighbour arithmetic
    // below would visit it twice.
    let corners = match (ring.first(), ring.last()) {
        (Some(first), Some(last)) if first == last => &ring[..ring.len().saturating_sub(1)],
        _ => ring,
    };
    if corners.len() < 3 {
        return None;
    }

    let lowest = lowest_corner(corners)?;
    let before = corners[wrapped_before(lowest, corners.len())];
    let at = corners[lowest];
    let after = corners[wrapped_after(lowest, corners.len())];

    if orientation(before, at, after) == Orientation::Collinear {
        return None;
    }

    let intruder = farthest_inside(corners, before, at, after);
    Some(match intruder {
        None => Fine::midway(Fine::of(before), Fine::of(after)),
        Some(corner) => Fine::midway(Fine::of(at), Fine::of(corner)),
    })
}

fn lowest_corner(corners: &[Snapped]) -> Option<usize> {
    corners
        .iter()
        .enumerate()
        .min_by_key(|(_, corner)| (corner.latitude_units(), corner.longitude_units()))
        .map(|(index, _)| index)
}

const fn wrapped_before(index: usize, count: usize) -> usize {
    match index.checked_sub(1) {
        Some(before) => before,
        None => count.saturating_sub(1),
    }
}

fn wrapped_after(index: usize, count: usize) -> usize {
    let next = index.saturating_add(1);
    if next == count { 0 } else { next }
}

/// The corner inside triangle `before`-`at`-`after` that is farthest from the
/// line `before`→`after`, if any corner is inside it at all.
///
/// Distance is compared by twice the triangle's area rather than by a length:
/// the two share a denominator, so the ordering is the same and the comparison
/// stays an integer one.
fn farthest_inside(
    corners: &[Snapped],
    before: Snapped,
    at: Snapped,
    after: Snapped,
) -> Option<Snapped> {
    let mut best: Option<(i128, Snapped)> = None;
    for corner in corners {
        if *corner == before || *corner == at || *corner == after {
            continue;
        }
        if !strictly_inside(before, at, after, *corner) {
            continue;
        }
        let reach = cross(before, after, *corner).abs();
        if best.is_none_or(|(held, _)| reach > held) {
            best = Some((reach, *corner));
        }
    }
    best.map(|(_, corner)| corner)
}

fn strictly_inside(one: Snapped, two: Snapped, three: Snapped, of: Snapped) -> bool {
    let sides = [
        orientation(one, two, of),
        orientation(two, three, of),
        orientation(three, one, of),
    ];
    !sides.contains(&Orientation::Collinear) && sides[0] == sides[1] && sides[1] == sides[2]
}

fn cross(from: Snapped, to: Snapped, of: Snapped) -> i128 {
    let edge_longitude =
        i128::from(to.longitude_units()).saturating_sub(i128::from(from.longitude_units()));
    let edge_latitude =
        i128::from(to.latitude_units()).saturating_sub(i128::from(from.latitude_units()));
    let to_longitude =
        i128::from(of.longitude_units()).saturating_sub(i128::from(from.longitude_units()));
    let to_latitude =
        i128::from(of.latitude_units()).saturating_sub(i128::from(from.latitude_units()));
    edge_longitude
        .saturating_mul(to_latitude)
        .saturating_sub(edge_latitude.saturating_mul(to_longitude))
}
