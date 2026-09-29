//! What a ring, a hole and a set of polygons must be before a shape is accepted.

use super::{Defect, Refused, Site, Step, adjacent, direction_is_stated, edge};
use crate::grid::{OffGrid, Snapped};
use crate::predicate::{Containment, ring_contains, segments_cross, segments_meet};
use crate::relate::areas_share_area;
use crate::shape::Area;
use tessari_types::Polygon;

/// Whether every edge of a path says which way round the world it goes.
///
/// The unit is the **edge**, not the box around the shape. The ambiguity is a
/// property of a pair of consecutive positions, and a box is only a consequence
/// of them — which is also what lets the refusal name the offending pair, and
/// what keeps a shape that legitimately spans the world: a cap reaching over the
/// pole has a box 360° wide and no edge wider than the gap between two of its
/// own corners.
pub(crate) fn directions_are_stated(positions: &[Snapped]) -> Result<(), Refused> {
    for (index, pair) in positions.windows(2).enumerate() {
        if !direction_is_stated(pair[0], pair[1]) {
            return Err(Refused::malformed(
                Defect::EdgeSpansHalfTheWorld {
                    from: pair[0].to_position(),
                    to: pair[1].to_position(),
                },
                Site::at(Step::Position(index)),
            ));
        }
    }
    Ok(())
}

pub(crate) fn no_repeats(positions: &[Snapped]) -> Result<(), Refused> {
    for (index, pair) in positions.windows(2).enumerate() {
        if pair[0] == pair[1] {
            return Err(Refused::malformed(
                Defect::RepeatedPosition {
                    position: pair[0].to_position(),
                },
                Site::at(Step::Position(index.saturating_add(1))),
            ));
        }
    }
    Ok(())
}

/// Whether any two non-adjacent edges of a closed ring meet.
///
/// Compared pairwise, but only among edges whose longitude spans overlap. Edges
/// are visited westernmost end first, and an edge leaves the active set once its
/// eastern end is behind the sweep. Two segments that meet must overlap in
/// longitude, so nothing is skipped and the answer is the same one an exhaustive
/// scan gives.
///
/// The worst case is still every pair — a ring whose edges all span the same
/// longitudes — but a boundary traced from real data is near-linear here, and the
/// alternative is a quadratic scan on the write path of every coastline.
///
/// Adjacent edges are excluded because they share a corner by construction, and
/// that includes the pair that closes the ring.
pub(crate) fn ring_meets_itself(ring: &[Snapped]) -> Option<(Snapped, Snapped)> {
    let count = ring.len().saturating_sub(1);
    if count < 4 {
        // With three edges every pair is adjacent, so there is nothing to find.
        return None;
    }

    let west = |index: usize| {
        let (from, to) = edge(ring, index);
        from.longitude_units().min(to.longitude_units())
    };
    let east = |index: usize| {
        let (from, to) = edge(ring, index);
        from.longitude_units().max(to.longitude_units())
    };

    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by_key(|&index| west(index));

    let mut active: Vec<usize> = Vec::new();
    for &index in &order {
        let sweep = west(index);
        active.retain(|&other| east(other) >= sweep);
        let (from, to) = edge(ring, index);
        for &other in &active {
            if adjacent(index, other, count) {
                continue;
            }
            let (other_from, other_to) = edge(ring, other);
            if segments_meet(from, to, other_from, other_to) {
                return Some((from, other_from));
            }
        }
        active.push(index);
    }
    None
}

/// Whether a hole lies wholly within its shell.
///
/// Two questions rather than one. Every corner must be inside or on the boundary,
/// which catches a hole that is simply somewhere else. And no edge may cross the
/// shell, which catches the case corners cannot see: a hole whose corners all sit
/// inside a concave shell while an edge between two of them passes out through the
/// notch and back.
///
/// Crossing here means a proper crossing. A hole is allowed to touch its shell —
/// that is a legal tangency, not an escape.
pub(crate) fn hole_sits_in(shell: &[Snapped], hole: &[Snapped]) -> Result<(), Refused> {
    for position in hole {
        if ring_contains(shell, *position) == Containment::Outside {
            return Err(Refused::malformed(
                Defect::HoleOutsideShell {
                    position: position.to_position(),
                },
                Site::whole(),
            ));
        }
    }
    if let Some(position) = first_crossing(hole, shell) {
        return Err(Refused::malformed(
            Defect::HoleOutsideShell {
                position: position.to_position(),
            },
            Site::whole(),
        ));
    }
    Ok(())
}

/// Whether two holes of one polygon share any area.
///
/// Either their edges cross, or one sits entirely within the other — the second
/// is invisible to an edge test and is the shape of a hole someone cut twice.
pub(crate) fn holes_are_separate(one: &[Snapped], other: &[Snapped]) -> Result<(), Refused> {
    if let Some(position) = first_crossing(one, other) {
        return Err(Refused::malformed(
            Defect::HolesOverlap {
                position: position.to_position(),
            },
            Site::whole(),
        ));
    }
    for (ring, probe) in [(one, other), (other, one)] {
        if let Some(corner) = probe.first()
            && ring_contains(ring, *corner) == Containment::Inside
        {
            return Err(Refused::malformed(
                Defect::HolesOverlap {
                    position: corner.to_position(),
                },
                Site::whole(),
            ));
        }
    }
    Ok(())
}

/// The start of the first edge of `one` that properly crosses an edge of `other`.
pub(crate) fn first_crossing(one: &[Snapped], other: &[Snapped]) -> Option<Snapped> {
    for index in 0..one.len().saturating_sub(1) {
        let (from, to) = edge(one, index);
        for against in 0..other.len().saturating_sub(1) {
            let (other_from, other_to) = edge(other, against);
            if segments_cross(from, to, other_from, other_to) {
                return Some(from);
            }
        }
    }
    None
}

/// Whether the members of a multi-polygon keep out of each other's way.
///
/// # Why this is checked here rather than left to the caller
///
/// It used to be left. Wave 98 recorded the omission and named the condition
/// under which it would matter — "the moment an area or an overlay is computed"
/// — and wave 99 made it matter one wave later by a route that sentence did not
/// anticipate: [`crate::relate::covers`] rules that a segment crossing a ring
/// edge transversally has left the region, which is exact for members with
/// disjoint interiors and wrong for members that overlap.
///
/// So the invariant a predicate rests on is now enforced where the value enters
/// the store, rather than assumed at the place that reads it.
///
/// # What counts as in each other's way
///
/// Shared **area**, and also a shared **stretch of edge**. Meeting at a corner
/// or crossing at a single point is legal and stays legal; RFC 7946 and OGC both
/// ask members to meet at finitely many points, so a shared line is already
/// outside the format.
///
/// # Errors
///
/// Returns [`Defect::MembersOverlap`] naming the two members, by their position
/// in the collection.
pub(crate) fn members_are_separate(members: &[Polygon]) -> Result<(), Refused> {
    let areas = members
        .iter()
        .map(Area::of)
        .collect::<Result<Vec<_>, OffGrid>>()?;
    for (later, area) in areas.iter().enumerate() {
        for (earlier, before) in areas.iter().take(later).enumerate() {
            if areas_share_area(before, area) {
                return Err(Refused::malformed(
                    Defect::MembersOverlap { earlier, later },
                    Site::whole(),
                ));
            }
        }
    }
    Ok(())
}
