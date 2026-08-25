//! How two shapes stand to each other.
//!
//! # Three algorithms, and everything else is derived from them
//!
//! There are exactly three questions here that need an algorithm. Every
//! predicate the language offers is one of them, or a composition of them with
//! [`Shape::boundary`]:
//!
//! - **do they meet at all** — [`intersects`];
//! - **is one wholly inside the other** — [`covers`];
//! - **do their interiors meet** — [`interiors_meet`].
//!
//! Then `disjoint` is the negation of the first, `covered_by` is the second with
//! its arguments swapped, `equals` is the second both ways round, `contains` is
//! the second *minus the boundary case*, and `touches` is the first **and not**
//! the third. Deriving them rather than writing seven separate routines is not
//! tidiness: seven routines are seven places for the boundary convention to be
//! decided differently, and the difference would surface as a row that appears
//! under one predicate and not under another.
//!
//! The third arrived last, with `touches`, and it is the one that cannot be
//! composed from the other two. It also cannot reuse their flattening: `Parts`
//! turns a shape into loose positions and loose segments, and an interior needs
//! to know where each **path** ends, since a path's interior is the path minus
//! its two ends. So it walks the shape into whole [`Part`]s of its own.
//!
//! # The boundary is where `contains` and `covers` part
//!
//! A position on a polygon's edge is **covered by** the polygon and **not
//! contained in** it. That is the DE-9IM convention and it is deliberate: the
//! boundary is a real place, and a predicate layer that folded it into inside or
//! outside could express only one of the two questions people actually ask.
//!
//! Here that difference is one line — `contains(a, b)` is `covers(a, b)` and not
//! `covers(a.boundary(), b)` — which is why the two can never drift apart.
//!
//! # Why a segment inside a region is the hard part
//!
//! Deciding that a *position* is in a region is a winding number, and this crate
//! has had one since the kernel was written. Deciding that a whole *segment* is
//! in a region is harder, because a segment can leave the region and come back
//! between its two ends, and testing the ends proves nothing.
//!
//! The exact answer normally needs the points where the segment crosses the
//! region's boundary, and those are **rational**, not grid points — which is the
//! reason engines built on floating point end up with a tolerance here.
//!
//! [`segment_in`] avoids them instead. If any boundary edge *properly* crosses
//! the segment, the segment has left the region and the answer is already no. If
//! none does, then every place the segment meets the boundary is one of its own
//! ends or a **vertex** of the region lying on it — all of them grid points. So
//! the segment is cut at those, and the midpoint of each piece is tested. A
//! midpoint of two grid points is a half unit, which is exactly what
//! [`crate::predicate::Fine`] exists to hold, so the whole test stays integral
//! and exact.
//!
//! Cutting at *every* touch rather than testing one midpoint of the whole
//! segment is the part that matters: a segment that leaves a region through one
//! notch and re-enters through another has a midpoint that is comfortably inside.
//!
//! # What is assumed rather than checked
//!
//! A multi-polygon whose members overlap or share a boundary stretch is not a
//! valid multi-polygon, and [`crate::accept`] refuses one — the assumption these
//! predicates rest on is enforced where the value enters the store rather than
//! trusted where it is read. Where it still matters, they answer as though the
//! members are disjoint: a shape that reached them another way and violates it
//! can produce a wrong `covers`, while `intersects` is unaffected.

use crate::grid::Snapped;
use crate::predicate::{
    Containment, Fine, Orientation, on_segment, on_segment_fine, ring_contains_fine,
    segments_cross, segments_meet,
};
use crate::shape::{Area, Shape};
use crate::witness::inside_ring;

/// Whether the two shapes share any position at all.
///
/// Exact for every pair of shapes. Boundaries count: two polygons meeting along
/// an edge intersect, and a position on a polygon's edge intersects it.
#[must_use]
pub fn intersects(one: &Shape, other: &Shape) -> bool {
    let (Some(one_box), Some(other_box)) = (one.bounds(), other.bounds()) else {
        // A shape holding no position meets nothing, itself included.
        return false;
    };
    if !one_box.meets(other_box) {
        return false;
    }

    let here = Parts::of(one);
    let there = Parts::of(other);

    // A vertex of either shape lying anywhere on the other. This one test covers
    // three cases that would otherwise be written separately and got wrong
    // separately: a shared position, a position on an edge, and a position
    // inside an area.
    if here.positions.iter().any(|at| there.holds(Fine::of(*at)))
        || there.positions.iter().any(|at| here.holds(Fine::of(*at)))
    {
        return true;
    }

    // Two boundary pieces crossing without sharing a vertex.
    here.segments.iter().any(|(from, to)| {
        there
            .segments
            .iter()
            .any(|(other_from, other_to)| segments_meet(*from, *to, *other_from, *other_to))
    })
}

/// Whether the two shapes share no position at all.
#[must_use]
pub fn disjoint(one: &Shape, other: &Shape) -> bool {
    !intersects(one, other)
}

/// Whether `one` holds the whole of `other`, boundary included.
///
/// The permissive half of the containment pair: a shape lying entirely on
/// `one`'s boundary is covered by it. See [`contains`] for the other half.
#[must_use]
pub fn covers(one: &Shape, other: &Shape) -> bool {
    let Some(other_box) = other.bounds() else {
        // Nothing is inside anything, vacuously — including inside another
        // empty shape.
        return true;
    };
    let Some(one_box) = one.bounds() else {
        return false;
    };
    if !one_box.holds(other_box) {
        return false;
    }

    let here = Parts::of(one);

    if !covers_positions(&here, other) || !covers_segments(&here, other) {
        return false;
    }
    covers_areas(&here, other)
}

/// Whether the whole of `one` lies inside `other`, boundary included.
#[must_use]
pub fn covered_by(one: &Shape, other: &Shape) -> bool {
    covers(other, one)
}

/// Whether `one` holds the whole of `other` **and** touches more than its edge.
///
/// The strict half of the containment pair. A position on a polygon's boundary
/// is covered by the polygon and not contained in it, which is the DE-9IM
/// convention and the reason both predicates exist.
#[must_use]
pub fn contains(one: &Shape, other: &Shape) -> bool {
    covers(one, other) && !covers(&one.boundary(), other)
}

/// Whether the whole of `one` lies inside `other`, not merely on its edge.
#[must_use]
pub fn within(one: &Shape, other: &Shape) -> bool {
    contains(other, one)
}

/// Whether the two shapes meet **and** their interiors do not.
///
/// The DE-9IM predicate, and the only one here that is not a composition of the
/// other two algorithms. It is written as one — `intersects` and not
/// [`interiors_meet`] — so the boundary convention still lives in a single
/// place, for the same reason `disjoint`, `covered_by`, `contains`, `within` and
/// `equals` are derived rather than written out.
///
/// Two consequences fall out of the definition rather than being special cases.
/// **Two positions never touch**: a position's interior is itself, so meeting at
/// all is meeting on the inside. OGC declares the predicate undefined for
/// point/point and `false` is how that is spelled here. **A position touches a
/// path only at an end**: everywhere else on the path is the path's interior.
#[must_use]
pub fn touches(one: &Shape, other: &Shape) -> bool {
    intersects(one, other) && !interiors_meet(one, other)
}

/// Whether the two shapes cover exactly the same positions.
///
/// A shape written with an extra collinear vertex equals the same shape without
/// it — the question is about the set of positions, not about how it was typed.
#[must_use]
pub fn equals(one: &Shape, other: &Shape) -> bool {
    covers(one, other) && covers(other, one)
}

// ------------------------------------------------------- the two algorithms

/// One shape flattened into the three things a predicate asks about.
///
/// Built once per call rather than per test: the positions and segments of a
/// coastline are walked once, and every pairwise question below reads the same
/// two vectors.
struct Parts<'a> {
    positions: Vec<Snapped>,
    segments: Vec<(Snapped, Snapped)>,
    areas: Vec<&'a Area>,
}

impl<'a> Parts<'a> {
    fn of(shape: &'a Shape) -> Self {
        let mut positions = Vec::new();
        shape.each_position(&mut |at| positions.push(at));
        let mut segments = Vec::new();
        shape.each_segment(&mut |from, to| segments.push((from, to)));
        let mut areas = Vec::new();
        shape.each_area(&mut |area| areas.push(area));
        Self {
            positions,
            segments,
            areas,
        }
    }

    /// Whether the position is anywhere in this shape — on a vertex, on an edge,
    /// or in an area.
    fn holds(&self, at: Fine) -> bool {
        self.positions.iter().any(|vertex| Fine::of(*vertex) == at)
            || self
                .segments
                .iter()
                .any(|(from, to)| on_segment_fine(Fine::of(*from), Fine::of(*to), at))
            || self
                .areas
                .iter()
                .any(|area| area_holds(area, at) != Containment::Outside)
    }
}

/// Where a position sits with respect to an area, its holes taken out.
fn area_holds(area: &Area, at: Fine) -> Containment {
    match ring_contains_fine(&area.shell, at) {
        Containment::Outside => Containment::Outside,
        Containment::Boundary => Containment::Boundary,
        Containment::Inside => {
            for hole in &area.holes {
                match ring_contains_fine(hole, at) {
                    // Inside a hole is outside the area; on a hole's edge is on
                    // the area's boundary, because a hole's edge is one.
                    Containment::Inside => return Containment::Outside,
                    Containment::Boundary => return Containment::Boundary,
                    Containment::Outside => {}
                }
            }
            Containment::Inside
        }
    }
}

/// Whether the closed segment lies wholly inside the shape.
///
/// The algorithm the module documentation describes. Exact, and integral
/// throughout.
fn segment_in(here: &Parts<'_>, from: Snapped, to: Snapped) -> bool {
    if from == to {
        return here.holds(Fine::of(from));
    }

    // A boundary edge crossed transversally means the segment left the region.
    // Touching does not: a segment resting against an edge has not left.
    for area in &here.areas {
        for ring in area.rings() {
            for edge in ring.windows(2) {
                if segments_cross(from, to, edge[0], edge[1]) {
                    return false;
                }
            }
        }
    }

    // With no crossing, every meeting point is a grid point: an end of this
    // segment, or a vertex of the shape lying on it.
    let mut touches = vec![from, to];
    for vertex in &here.positions {
        if *vertex != from && *vertex != to && on_segment(from, to, *vertex) {
            touches.push(*vertex);
        }
    }
    order_along(&mut touches, from, to);
    touches.dedup();

    touches
        .windows(2)
        .all(|piece| here.holds(Fine::midway(Fine::of(piece[0]), Fine::of(piece[1]))))
}

/// Put collinear positions in the order the segment visits them.
///
/// Every position here lies on the segment, so one axis orders them all; the
/// axis that does is whichever one the segment actually moves along.
fn order_along(positions: &mut [Snapped], from: Snapped, to: Snapped) {
    let by_longitude = from.longitude_units() != to.longitude_units();
    let ascending = if by_longitude {
        to.longitude_units() > from.longitude_units()
    } else {
        to.latitude_units() > from.latitude_units()
    };
    positions.sort_by_key(|at| {
        let along = if by_longitude {
            at.longitude_units()
        } else {
            at.latitude_units()
        };
        if ascending {
            along
        } else {
            along.saturating_neg()
        }
    });
}

fn covers_positions(here: &Parts<'_>, other: &Shape) -> bool {
    let mut all_held = true;
    other.each_position(&mut |at| {
        if all_held && !here.holds(Fine::of(at)) {
            all_held = false;
        }
    });
    all_held
}

fn covers_segments(here: &Parts<'_>, other: &Shape) -> bool {
    let mut all_held = true;
    other.each_segment(&mut |from, to| {
        if all_held && !segment_in(here, from, to) {
            all_held = false;
        }
    });
    all_held
}

/// The part of `covers` that is about area rather than about boundary.
///
/// Once the covered shape's whole boundary is known to lie inside the covering
/// one, only one way to fail is left: a **hole** of the covering shape reaching
/// into the covered shape's interior. The hole's own vertices cannot show that —
/// they sit on the covering shape's boundary — so the hole's interior is
/// witnessed by a constructed position instead.
///
/// One witness is enough. The covered shape's boundary is already known not to
/// enter the hole, so the hole's interior is wholly inside the covered shape or
/// wholly outside it, and any interior position decides which.
fn covers_areas(here: &Parts<'_>, other: &Shape) -> bool {
    let mut covered_areas = Vec::new();
    other.each_area(&mut |area| covered_areas.push(area));
    if covered_areas.is_empty() {
        return true;
    }
    // A shape with no area of its own cannot hold one, however much of its
    // boundary happens to lie along the other's.
    if here.areas.is_empty() {
        return false;
    }

    for area in &here.areas {
        for hole in &area.holes {
            let Some(witness) = inside_ring(hole) else {
                continue;
            };
            if covered_areas
                .iter()
                .any(|covered| area_holds(covered, witness) == Containment::Inside)
            {
                return false;
            }
        }
    }
    true
}

// ------------------------------------------- the third algorithm: interiors

/// One piece of a shape, kept whole so that its own boundary is still visible.
///
/// [`Parts`] above flattens a shape into loose positions and loose segments,
/// which is all `intersects` and `covers` ever need. The interior question
/// cannot use it: a path's interior is the path **minus its two ends**, and once
/// the path has been split into segments there is no way to tell an end of the
/// path from a vertex in the middle of it.
enum Part<'a> {
    /// Interior: the position itself.
    Position(Snapped),
    /// Interior: the path minus its two ends — or all of it, when it is closed.
    Path(&'a [Snapped]),
    /// Interior: the area minus its rings.
    Area(&'a Area),
}

/// Whether any position lies inside both shapes.
///
/// Taken **part-wise**, which is the convention `Parts::of` already establishes
/// for a collection: the members of a collection are read as separate pieces
/// rather than as one region. DE-9IM over a heterogeneous collection is not
/// settled enough to claim otherwise, and inventing an answer here would be a
/// claim this module cannot support.
fn interiors_meet(one: &Shape, other: &Shape) -> bool {
    let mut here = Vec::new();
    parts_of(one, &mut here);
    let mut there = Vec::new();
    parts_of(other, &mut there);
    here.iter()
        .any(|part| there.iter().any(|against| share_interior(part, against)))
}

fn parts_of<'a>(shape: &'a Shape, into: &mut Vec<Part<'a>>) {
    match shape {
        Shape::Point(at) => into.push(Part::Position(*at)),
        Shape::MultiPoint(positions) => {
            into.extend(positions.iter().map(|at| Part::Position(*at)));
        }
        Shape::Line(path) => into.push(Part::Path(path)),
        Shape::MultiLine(paths) => into.extend(paths.iter().map(|path| Part::Path(path))),
        Shape::Polygon(area) => into.push(Part::Area(area)),
        Shape::MultiPolygon(areas) => into.extend(areas.iter().map(Part::Area)),
        Shape::Collection(members) => {
            for member in members {
                parts_of(member, into);
            }
        }
    }
}

/// The six pairings, each decided directly rather than through a dimension
/// algebra. Six cases written once are smaller than a general machinery, and
/// each carries its own argument for why it is exact.
fn share_interior(one: &Part<'_>, other: &Part<'_>) -> bool {
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

/// Whether the position lies on the path and is not one of its ends.
///
/// A **closed** path has no ends, so every position on it is interior — which is
/// why a ring drawn as a line touches nothing along its length.
fn inside_path(at: Snapped, path: &[Snapped]) -> bool {
    if !path.windows(2).any(|edge| on_segment(edge[0], edge[1], at)) {
        return false;
    }
    match (path.first(), path.last()) {
        (Some(first), Some(last)) if first != last => at != *first && at != *last,
        _ => true,
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
fn paths_share_interior(one: &[Snapped], other: &[Snapped]) -> bool {
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

/// Whether any position of the path is strictly inside the area.
///
/// The question is about the path's *interior*, but it is asked about the whole
/// path, and that is not a shortcut. An area's interior is open, so a path
/// position strictly inside it has a whole stretch of the path around it also
/// inside — and a stretch of positive length cannot be made only of the path's
/// two ends. A path with no length has no interior and answers `false`.
fn path_reaches_inside(path: &[Snapped], area: &Area) -> bool {
    path.windows(2)
        .any(|edge| segment_reaches_inside(edge[0], edge[1], area))
}

/// Whether any position of the closed segment is strictly inside the area.
///
/// Mirrors [`segment_in`] and inverts its verdict. A ring edge crossed
/// transversally puts a stretch of the segment on the ring's far side, which is
/// the area's inside for the shell and the area's inside for a hole alike — the
/// side that is not the hole is the area. Failing that, every meeting point is a
/// grid position, containment is constant between consecutive ones, and each
/// piece is decided by its midpoint as an exact half-unit.
fn segment_reaches_inside(from: Snapped, to: Snapped, area: &Area) -> bool {
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
fn areas_share_ground(one: &Area, other: &Area) -> bool {
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
fn inside_area(area: &Area) -> Option<Fine> {
    let holds = |at: Fine| (area_holds(area, at) == Containment::Inside).then_some(at);

    if let Some(witness) = inside_ring(&area.shell).and_then(&holds) {
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

fn ring_bounds(ring: &[Snapped]) -> Option<crate::bounds::Bounds> {
    let mut held: Option<crate::bounds::Bounds> = None;
    for position in ring {
        held = Some(match held {
            None => crate::bounds::Bounds::of_position(*position),
            Some(bounds) => bounds.widened_to(*position),
        });
    }
    held
}

fn edges_of(area: &Area) -> impl Iterator<Item = (Snapped, Snapped)> + '_ {
    area.rings()
        .flat_map(|ring| ring.windows(2).map(|edge| (edge[0], edge[1])))
}

/// Whether two segments lie along the same line and overlap over a positive
/// length — as opposed to meeting at a single point.
fn share_a_stretch(one: (Snapped, Snapped), other: (Snapped, Snapped)) -> bool {
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
