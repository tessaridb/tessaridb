//! How two areas, or a path and an area, share ground.

use super::{Part, area_holds, edges_of, inside_path, order_along, path_reaches_inside};
use crate::grid::Snapped;
use crate::predicate::{Containment, Fine, Orientation, on_segment, segments_cross};
use crate::shape::Area;
use crate::witness::inside_ring;

/// The six pairings, each decided directly rather than through a dimension
/// algebra. Six cases written once are smaller than a general machinery, and
/// each carries its own argument for why it is exact.
pub(crate) fn share_interior(one: &Part<'_>, other: &Part<'_>) -> bool {
    match (one, other) {
        (Part::Position(at), Part::Position(against)) => at == against,
        (Part::Position(at), Part::Path(path)) | (Part::Path(path), Part::Position(at)) => {
            inside_path(*at, path)
        }
        (Part::Position(at), Part::Area(area)) | (Part::Area(area), Part::Position(at)) => {
            area_holds(area, Fine::of(*at)) == Containment::Inside
        }
        (Part::Path(one), Part::Path(other)) => paths_share_interior(one, other),
        (Part::Path(path), Part::Area(area)) | (Part::Area(area), Part::Path(path)) => {
            path_reaches_inside(path, area)
        }
        (Part::Area(one), Part::Area(other)) => areas_share_ground(one, other),
    }
}

/// Whether two paths share a position interior to both.
///
/// Three cases, and none of them names a rational intersection point — the same
/// discipline [`segment_in`] keeps.
///
/// A **transversal crossing** meets at a position strictly inside both segments,
/// so it is strictly inside both paths whatever their ends are. A **collinear
/// overlap of positive length** holds infinitely many positions while the two
/// paths have at most four ends between them, so one of them must be interior to
/// both. Anything else is a finite set of **grid** positions: two grid segments
/// that meet without crossing meet at an end of one of them, and every such end
/// is a vertex of its path.
pub(crate) fn paths_share_interior(one: &[Snapped], other: &[Snapped]) -> bool {
    for first in one.windows(2) {
        for second in other.windows(2) {
            if segments_cross(first[0], first[1], second[0], second[1])
                || share_a_stretch((first[0], first[1]), (second[0], second[1]))
            {
                return true;
            }
        }
    }
    one.iter()
        .chain(other.iter())
        .any(|at| inside_path(*at, one) && inside_path(*at, other))
}

/// Whether any position of the closed segment is strictly inside the area.
///
/// Mirrors [`segment_in`] and inverts its verdict. A ring edge crossed
/// transversally puts a stretch of the segment on the ring's far side, which is
/// the area's inside for the shell and the area's inside for a hole alike — the
/// side that is not the hole is the area. Failing that, every meeting point is a
/// grid position, containment is constant between consecutive ones, and each
/// piece is decided by its midpoint as an exact half-unit.
pub(crate) fn segment_reaches_inside(from: Snapped, to: Snapped, area: &Area) -> bool {
    if from == to {
        return area_holds(area, Fine::of(from)) == Containment::Inside;
    }

    for ring in area.rings() {
        for edge in ring.windows(2) {
            if segments_cross(from, to, edge[0], edge[1]) {
                return true;
            }
        }
    }

    let mut touches = vec![from, to];
    for ring in area.rings() {
        for corner in ring {
            if *corner != from && *corner != to && on_segment(from, to, *corner) {
                touches.push(*corner);
            }
        }
    }
    order_along(&mut touches, from, to);
    touches.dedup();

    touches.windows(2).any(|piece| {
        area_holds(area, Fine::midway(Fine::of(piece[0]), Fine::of(piece[1])))
            == Containment::Inside
    })
}

/// Whether the interiors of two areas overlap.
///
/// **Not [`areas_share_area`]**, and the difference is the whole point of this
/// predicate: that one counts a shared boundary stretch as sharing, because
/// `accept` needs it to. Two areas meeting along an edge are exactly the case
/// `touches` exists to answer `true` for, and their interiors are disjoint.
///
/// A transversal crossing of two boundaries puts each area's interior on both
/// sides of the other's edge, so the interiors overlap. With no crossing the two
/// are nested, or they only touch, and two further tests separate those.
///
/// **A boundary position strictly inside the other area** settles it: an area
/// has interior arbitrarily close to every position on its own boundary, and the
/// other's interior is open, so the two interiors meet there. This is what finds
/// a nested pair, and it needs no witness.
///
/// **Otherwise the boundaries can only lie on each other**, and the two areas
/// either occupy the same ground or sit on opposite sides of a shared edge — a
/// square and the square that exactly fills its hole. Only an interior position
/// separates those, so one is constructed. A vertex cannot serve: a vertex sits
/// on its own area's boundary, never in its interior.
pub(crate) fn areas_share_ground(one: &Area, other: &Area) -> bool {
    let (Some(one_box), Some(other_box)) = (ring_bounds(&one.shell), ring_bounds(&other.shell))
    else {
        return false;
    };
    if !one_box.meets(other_box) {
        return false;
    }

    for first in edges_of(one) {
        for second in edges_of(other) {
            if segments_cross(first.0, first.1, second.0, second.1) {
                return true;
            }
        }
    }

    [(one, other), (other, one)]
        .into_iter()
        .any(|(inner, outer)| {
            edges_of(inner).any(|(from, to)| segment_reaches_inside(from, to, outer))
                || inside_area(inner)
                    .is_some_and(|witness| area_holds(outer, witness) == Containment::Inside)
        })
}

/// A position strictly inside the area, holes taken out.
///
/// [`inside_ring`] witnesses the **shell**, which is not the same thing: the ear
/// construction on a square shell returns its centre, and a square with a square
/// bite out of its middle has its centre in the bite. So the shell's witness is
/// a candidate rather than an answer, and when it falls in a hole the search
/// continues across the midpoints between the rings — a position between the
/// shell and a hole, or between two holes, is where such an area's ground
/// actually is.
///
/// Every candidate is the midpoint of two grid positions, so it is an exact
/// half-unit, and every candidate is checked rather than assumed. `None` means
/// no candidate landed inside, which for an area with ground is a shape whose
/// rings are arranged more awkwardly than anything the corpus holds; the
/// property test over the fixtures is what says so rather than this sentence.
pub(crate) fn inside_area(area: &Area) -> Option<Fine> {
    let holds = |at: Fine| (area_holds(area, at) == Containment::Inside).then_some(at);

    if let Some(witness) = inside_ring(&area.shell).and_then(holds) {
        return Some(witness);
    }
    for hole in &area.holes {
        for corner in hole {
            for outer in area.shell.iter().chain(area.holes.iter().flatten()) {
                if outer == corner {
                    continue;
                }
                if let Some(witness) = holds(Fine::midway(Fine::of(*corner), Fine::of(*outer))) {
                    return Some(witness);
                }
            }
        }
    }
    None
}

/// Whether two areas share any area at all — as opposed to touching.
///
/// # Why this is a separate question from `intersects`
///
/// Two areas that meet along an edge or at a corner intersect, and are still
/// perfectly legal as two members of one multi-polygon. What is not legal, and
/// what this asks about, is **shared area**: an overlap with a positive extent.
///
/// [`crate::accept`] needs the distinction because the containment rule this
/// module rests on assumes it. ADR-0029 Decision 2 rules that a segment crossing
/// a ring edge transversally has left the region — which is exact for members
/// whose interiors are disjoint, and wrong for members that overlap, since a
/// segment crossing from one member's interior into another's has not left the
/// shape at all.
///
/// A **shared boundary stretch** counts here too, even though it encloses no
/// area. Two members sharing an edge break the same rule for the same reason,
/// and OGC asks members to meet at finitely many points rather than along a
/// line, so refusing it is not a stricter reading than the standard's.
#[must_use]
pub fn areas_share_area(one: &Area, other: &Area) -> bool {
    let (Some(one_box), Some(other_box)) = (ring_bounds(&one.shell), ring_bounds(&other.shell))
    else {
        return false;
    };
    if !one_box.meets(other_box) {
        return false;
    }

    for first in edges_of(one) {
        for second in edges_of(other) {
            if segments_cross(first.0, first.1, second.0, second.1)
                || share_a_stretch(first, second)
            {
                return true;
            }
        }
    }

    // With no crossing and no shared stretch the two are nested or apart, and a
    // nested one has every vertex strictly inside the other.
    one.shell
        .iter()
        .any(|corner| area_holds(other, Fine::of(*corner)) == Containment::Inside)
        || other
            .shell
            .iter()
            .any(|corner| area_holds(one, Fine::of(*corner)) == Containment::Inside)
}

pub(crate) fn ring_bounds(ring: &[Snapped]) -> Option<crate::bounds::Bounds> {
    let mut held: Option<crate::bounds::Bounds> = None;
    for position in ring {
        held = Some(match held {
            None => crate::bounds::Bounds::of_position(*position),
            Some(bounds) => bounds.widened_to(*position),
        });
    }
    held
}

/// Whether two segments lie along the same line and overlap over a positive
/// length — as opposed to meeting at a single point.
pub(crate) fn share_a_stretch(one: (Snapped, Snapped), other: (Snapped, Snapped)) -> bool {
    let collinear = |of: Snapped| crate::predicate::orientation(one.0, one.1, of);
    if collinear(other.0) != Orientation::Collinear || collinear(other.1) != Orientation::Collinear
    {
        return false;
    }
    // Collinear: they overlap over a stretch when each has an end strictly
    // inside the other, or when one contains both of the other's ends.
    let inside_one = |of: Snapped| on_segment(one.0, one.1, of);
    let inside_other = |of: Snapped| on_segment(other.0, other.1, of);
    let shared: Vec<Snapped> = [one.0, one.1, other.0, other.1]
        .into_iter()
        .filter(|position| inside_one(*position) && inside_other(*position))
        .collect();
    shared
        .iter()
        .any(|position| shared.iter().any(|another| another != position))
}
