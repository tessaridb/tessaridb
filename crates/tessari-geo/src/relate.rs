//! How two shapes stand to each other.
//!
//! # Two algorithms, and everything else is derived from them
//!
//! There are exactly two questions here that need an algorithm. Every predicate
//! the language offers is one of them, or a composition of them with
//! [`Shape::boundary`]:
//!
//! - **do they meet at all** — [`intersects`];
//! - **is one wholly inside the other** — [`covers`].
//!
//! Then `disjoint` is the negation of the first, `covered_by` is the second with
//! its arguments swapped, `equals` is the second both ways round, and `contains`
//! is the second *minus the boundary case*. Deriving them rather than writing
//! six separate routines is not tidiness: six routines are six places for the
//! boundary convention to be decided differently, and the difference would
//! surface as a row that appears under one predicate and not under another.
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
//! valid multi-polygon, and the ingest boundary does not yet refuse one
//! (recorded as an open question). Where it matters, these predicates answer as
//! though the members are disjoint. A stored shape that violates it can produce
//! a wrong `covers`; `intersects` is unaffected.

use crate::grid::Snapped;
use crate::predicate::{
    Containment, Fine, on_segment, on_segment_fine, ring_contains_fine, segments_cross,
    segments_meet,
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
