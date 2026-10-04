//! The boundary a shape crosses to get into the store.
//!
//! # Two steps, and the order is not the intuitive one
//!
//! A shape is **snapped first and judged second**, and that order is load-bearing
//! rather than incidental.
//!
//! Snapping is a transformation, and it can create invalidity. Two positions
//! distinct in `f64` can land on one grid point, and when they do a ring's last
//! distinct vertex can coincide with its first, two nearly-touching edges can
//! become genuinely touching, and a hole just inside its shell can land exactly
//! on the boundary. So a shape that is well formed as written can be malformed
//! as stored.
//!
//! Judge first and every one of those is accepted and written, and the store then
//! holds geometry that fails its own definition — which is the exact failure the
//! check exists to prevent, arriving through the door the check left open. Judge
//! second and the shape being judged is the shape that will be stored.
//!
//! # Why the check is here at all rather than in the schema
//!
//! The storage layer's schema check inspects an already-encoded payload, so it
//! cannot snap: snapping is a transformation and the payload is downstream of it.
//! It also has nothing to say about a table whose schema constrains nothing,
//! which is the store's default shape. Validity is a property of the value, not
//! of a declaration about it, so the boundary is where the value is finalised and
//! it applies whether or not any field says `TYPE geometry`.
//!
//! # Refused, never quietly repaired
//!
//! Every repair strategy preserves some of node positions, area and topology at
//! the cost of the others, and none preserves all three. A store that picked one
//! silently would return a shape nobody wrote, and a later comparison against the
//! source system would find a difference nobody could explain. So a defect is
//! named, located, and handed back.
//!
//! A refusal reports the **snapped** coordinates, because those are the ones the
//! complaint is about. A caller comparing them against what it sent can see that
//! quantisation was the cause; a message quoting the submitted coordinates back
//! would describe a shape that was never in question.
//!
//! # Refused, too, when the coordinates do not say what shape they mean
//!
//! Most of the rules here are about a shape being well formed. One is not. An
//! edge more than half the world wide in longitude can be joined two ways, and
//! the wrapped one is the shorter — so a polygon written from 179°E to 179°W is
//! a narrow strip to the person who wrote it and a band round the rest of the
//! planet to anything reading the coordinates as written.
//!
//! The store reads them as written, and its box and its predicates agree with
//! each other about that reading, so nothing here is inconsistent. What is
//! missing is the caller's intent, and the two candidates are not near-misses:
//! one is the complement of the other. Choosing silently would be the failure
//! this boundary exists to prevent, arriving as a shape nobody wrote.
//!
//! So the edge is refused, for the same reason a bowtie is: a shape with two
//! possible readings has none the store can keep. Both intended shapes remain
//! sayable — split the geometry at ±180 for the short way, which is what
//! RFC 7946 asks producers to do anyway, or put one position between the ends
//! for the long way, after which every edge is under half the world and the
//! reading is unique.
//!
//! # What is deliberately not checked here
//!
//! Ring winding order is not enforced. RFC 7946 asks an exterior ring to run
//! counter-clockwise, and also tells parsers not to reject rings that do not —
//! normalising would be a repair, and this boundary does not repair.
//!
//! The members of a multi-polygon **are** checked against each other, and that
//! is the one rule here whose reason lives in another module. It used to be
//! omitted, harmlessly: an overlap was a property of the collection rather than
//! of any shape in it, and nothing rested on it. [`crate::relate::covers`] does
//! — its rule that a segment crossing a ring edge has left the region is exact
//! for members with disjoint interiors and wrong otherwise — so the invariant is
//! enforced where the value enters rather than assumed where it is read.

mod rings;
use tessari_types::{Geometry, Polygon, Position, Ring};

use crate::bounds::Bounds;
pub use crate::error::{Defect, Refused, Site, Step};
use crate::grid::{OffGrid, Snapped};
use crate::predicate::twice_signed_area;
pub(crate) use rings::{
    directions_are_stated, hole_sits_in, holes_are_separate, members_are_separate, no_repeats,
    ring_meets_itself,
};

/// Put a shape on the grid, and decide whether the store will hold it.
///
/// Returns the shape as it would be stored: every position on a grid point, so
/// accepting it again moves nothing.
///
/// # Errors
///
/// Returns [`Refused`] when a coordinate is off the sphere, or when the snapped
/// shape breaks one of the rules in [`Defect`].
pub fn accept(shape: &Geometry) -> Result<Geometry, Refused> {
    accept_shape(shape)
}

fn accept_shape(shape: &Geometry) -> Result<Geometry, Refused> {
    Ok(match shape {
        Geometry::Point(position) => Geometry::Point(Snapped::of(*position)?.to_position()),
        Geometry::MultiPoint(positions) => Geometry::MultiPoint(degrees(&snap_all(positions)?)),
        Geometry::Line(positions) => Geometry::Line(degrees(&accept_line(positions)?)),
        Geometry::MultiLine(lines) => Geometry::MultiLine(
            lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    accept_line(line)
                        .map(|snapped| degrees(&snapped))
                        .map_err(|refused| refused.under(Step::Member(index)))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Geometry::Polygon(polygon) => Geometry::Polygon(accept_polygon(polygon)?),
        Geometry::MultiPolygon(polygons) => {
            let members = polygons
                .iter()
                .enumerate()
                .map(|(index, polygon)| {
                    accept_polygon(polygon).map_err(|refused| refused.under(Step::Member(index)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            members_are_separate(&members)?;
            Geometry::MultiPolygon(members)
        }
        Geometry::Collection(shapes) => Geometry::Collection(
            shapes
                .iter()
                .enumerate()
                .map(|(index, inner)| {
                    accept_shape(inner)
                        .map(Box::new)
                        .map_err(|refused| refused.under(Step::Member(index)))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
    })
}

fn accept_line(positions: &[Position]) -> Result<Vec<Snapped>, Refused> {
    let snapped = snap_all(positions)?;
    if snapped.len() < 2 {
        return Err(Refused::malformed(
            Defect::LineTooShort { had: snapped.len() },
            Site::whole(),
        ));
    }
    no_repeats(&snapped)?;
    directions_are_stated(&snapped)?;
    Ok(snapped)
}

fn accept_polygon(polygon: &Polygon) -> Result<Polygon, Refused> {
    let shell = accept_ring(&polygon.exterior).map_err(|refused| refused.under(Step::Shell))?;

    let mut holes: Vec<Vec<Snapped>> = Vec::with_capacity(polygon.interiors.len());
    for (index, interior) in polygon.interiors.iter().enumerate() {
        let step = Step::Hole(index);
        let hole = accept_ring(interior).map_err(|refused| refused.under(step))?;
        hole_sits_in(&shell, &hole).map_err(|refused| refused.under(step))?;
        for earlier in &holes {
            holes_are_separate(earlier, &hole).map_err(|refused| refused.under(step))?;
        }
        holes.push(hole);
    }

    Ok(Polygon {
        exterior: Ring(degrees(&shell)),
        interiors: holes.iter().map(|hole| Ring(degrees(hole))).collect(),
    })
}

/// The checks a ring passes, in the order that gives the most useful answer.
///
/// Structure first, because a ring that does not close has no other property
/// worth reporting. Then the repeats snapping creates, then the edges whose
/// direction round the world is not stated, then area, then self-intersection —
/// so a sliver that collapsed under the grid is described as enclosing nothing
/// rather than as crossing itself, which is the same fact told the less helpful
/// way, and a ring reaching the wrong way round the planet is described as
/// ambiguous rather than by whatever that reading happens to do to its area.
fn accept_ring(ring: &Ring) -> Result<Vec<Snapped>, Refused> {
    let snapped = snap_all(&ring.0)?;
    if snapped.len() < 4 {
        return Err(Refused::malformed(
            Defect::RingTooShort { had: snapped.len() },
            Site::whole(),
        ));
    }
    if snapped.first() != snapped.last() {
        return Err(Refused::malformed(Defect::RingNotClosed, Site::whole()));
    }
    no_repeats(&snapped)?;
    directions_are_stated(&snapped)?;
    if twice_signed_area(&snapped) == 0 {
        return Err(Refused::malformed(Defect::RingHasNoArea, Site::whole()));
    }
    if let Some((from, meets)) = ring_meets_itself(&snapped) {
        return Err(Refused::malformed(
            Defect::RingSelfIntersects {
                from: from.to_position(),
                meets: meets.to_position(),
            },
            Site::whole(),
        ));
    }
    Ok(snapped)
}

// ------------------------------------------------------------ the checks

/// Whether one edge's own coordinates say which way round the world it goes.
///
/// The threshold is **more than** half the world, strictly. Under 180° the
/// planar reading is the shorter of the two and is the only sensible one. At
/// exactly 180° the two readings have the same length and the same box, so
/// nothing a store can observe distinguishes them. Over 180° the planar reading
/// is the *longer* one, the wrapped reading is shorter, and they are different
/// shapes with different boxes — so the coordinates no longer state which was
/// meant.
///
/// Two positions at one pole are the exception, because there every longitude
/// is the same place and both readings are the same degenerate point. That is
/// the rule's own statement rather than a case bolted onto it, and without it a
/// polar cap would be unstorable for no correctness gained.
fn direction_is_stated(from: Snapped, to: Snapped) -> bool {
    if from.is_at_a_pole() && from.latitude_units() == to.latitude_units() {
        return true;
    }
    !Bounds::of_position(from)
        .widened_to(to)
        .spans_more_than_half_the_world()
}

// ------------------------------------------------------------- the plumbing

fn edge(positions: &[Snapped], index: usize) -> (Snapped, Snapped) {
    (positions[index], positions[index.saturating_add(1)])
}

fn adjacent(one: usize, other: usize, count: usize) -> bool {
    let gap = one.abs_diff(other);
    // A gap of one along the sequence, or the pair at the two ends, which meet
    // where the ring closes.
    gap == 1 || gap == count.saturating_sub(1)
}

fn snap_all(positions: &[Position]) -> Result<Vec<Snapped>, OffGrid> {
    positions.iter().copied().map(Snapped::of).collect()
}

fn degrees(positions: &[Snapped]) -> Vec<Position> {
    positions
        .iter()
        .map(|snapped| snapped.to_position())
        .collect()
}
