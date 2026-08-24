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

use tessari_types::{Geometry, Polygon, Position, Ring};

use crate::bounds::Bounds;
use crate::grid::{OffGrid, Snapped};
use crate::predicate::{
    Containment, ring_contains, segments_cross, segments_meet, twice_signed_area,
};
use crate::relate::areas_share_area;
use crate::shape::Area;

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

/// Why the store would not hold a shape.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Refused {
    /// A coordinate could not be put on the grid at all.
    #[error(transparent)]
    OffGrid(#[from] OffGrid),
    /// The snapped shape breaks a rule the store holds all its geometry to.
    #[error("{defect}, at {at}")]
    Malformed {
        /// What was wrong.
        defect: Defect,
        /// Where in the shape.
        at: Site,
    },
}

impl Refused {
    fn malformed(defect: Defect, at: Site) -> Self {
        Self::Malformed { defect, at }
    }

    /// Say that this happened one level further in than it currently reads.
    ///
    /// Only the error path pays for the location, which is why the site is built
    /// outward from the defect rather than threaded down through every call.
    fn under(self, step: Step) -> Self {
        match self {
            Self::Malformed { defect, mut at } => {
                at.0.insert(0, step);
                Self::Malformed { defect, at }
            }
            off_grid @ Self::OffGrid(_) => off_grid,
        }
    }
}

/// What was wrong with a snapped shape.
///
/// The list is closed. Every coordinate a variant carries is a **snapped**
/// coordinate — the store's version of the position, not the caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Defect {
    /// A path with fewer than two positions goes nowhere.
    #[error("a line needs two positions to be a path, and this one has {had}")]
    LineTooShort {
        /// How many it had.
        had: usize,
    },
    /// A ring needs three corners and the repeat that closes it.
    #[error("a ring needs four positions to bound an area, and this one has {had}")]
    RingTooShort {
        /// How many it had.
        had: usize,
    },
    /// The first and last positions of a ring differ.
    #[error("a ring must end where it began")]
    RingNotClosed,
    /// Two positions in a row are the same grid point.
    ///
    /// Very often this is snapping showing its work: the two were distinct as
    /// written and are one position at the store's resolution.
    #[error(
        "two positions in a row are the same grid point, longitude {} latitude {}",
        position.longitude,
        position.latitude
    )]
    RepeatedPosition {
        /// The grid point they both became.
        position: Position,
    },
    /// An edge whose direction round the world its own coordinates do not state.
    ///
    /// Two positions more than half the world apart in longitude can be joined
    /// two ways, and the wrapped one is the shorter. Coordinates are read as
    /// written, so the store would keep the longer path — which for a shape
    /// meant to cross the antimeridian is the complement of what was asked for,
    /// with nothing to report it. Neither reading is guessed.
    #[error(
        "an edge from longitude {} latitude {} to longitude {} latitude {} spans more than half the world, so which way round it goes is not stated: to cross the antimeridian, split the shape in two at ±180; to mean the long way round, put a position between the two ends",
        from.longitude,
        from.latitude,
        to.longitude,
        to.latitude
    )]
    EdgeSpansHalfTheWorld {
        /// Where the edge starts.
        from: Position,
        /// Where it ends.
        to: Position,
    },
    /// A ring that closes but encloses nothing.
    #[error("the ring encloses no area")]
    RingHasNoArea,
    /// Two edges of one ring meet where they should not.
    ///
    /// A crossing and a touch are both this: a ring that meets itself anywhere
    /// other than at a shared corner has an inside the store cannot name.
    #[error(
        "the ring meets itself: an edge from longitude {} latitude {} reaches an edge from longitude {} latitude {}",
        from.longitude,
        from.latitude,
        meets.longitude,
        meets.latitude
    )]
    RingSelfIntersects {
        /// Where one of the two edges starts.
        from: Position,
        /// Where the other starts.
        meets: Position,
    },
    /// A hole reaches outside the shell it is cut from.
    #[error(
        "a hole reaches outside its shell, at longitude {} latitude {}",
        position.longitude,
        position.latitude
    )]
    HoleOutsideShell {
        /// A position on the part that is outside.
        position: Position,
    },
    /// Two holes of one polygon share area.
    #[error(
        "two holes of one polygon overlap, at longitude {} latitude {}",
        position.longitude,
        position.latitude
    )]
    HolesOverlap {
        /// A position in the shared part.
        position: Position,
    },
    /// Two members of a multi-polygon share area, or share a stretch of edge.
    ///
    /// Named by position in the collection rather than by coordinate: what is
    /// wrong is the pair, and the place where they meet is a rational point
    /// rather than a grid one, so a coordinate here would be a rounded
    /// approximation of the complaint.
    #[error("member {earlier} and member {later} of a multi-polygon share area")]
    MembersOverlap {
        /// The earlier of the two, counting from zero.
        earlier: usize,
        /// The later of the two.
        later: usize,
    },
}

/// Where in a shape a defect is.
///
/// Outermost step first, so it reads the way a person would point: the second
/// member, its first hole, its fifth position.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Site(Vec<Step>);

impl Site {
    /// A defect that belongs to the shape as a whole.
    #[must_use]
    pub const fn whole() -> Self {
        Self(Vec::new())
    }

    /// A defect one step in.
    #[must_use]
    pub fn at(step: Step) -> Self {
        Self(vec![step])
    }

    /// The steps, outermost first.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.0
    }
}

impl core::fmt::Display for Site {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_empty() {
            return f.write_str("the shape itself");
        }
        for (index, step) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" → ")?;
            }
            write!(f, "{step}")?;
        }
        Ok(())
    }
}

/// One step into a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// The nth shape of a multi-shape or a collection, counting from zero.
    Member(usize),
    /// A polygon's outer ring.
    Shell,
    /// The nth ring cut out of a polygon, counting from zero.
    Hole(usize),
    /// The nth position, counting from zero.
    Position(usize),
}

impl core::fmt::Display for Step {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Member(index) => write!(f, "member {index}"),
            Self::Shell => f.write_str("the shell"),
            Self::Hole(index) => write!(f, "hole {index}"),
            Self::Position(index) => write!(f, "position {index}"),
        }
    }
}

// ------------------------------------------------------------ the shapes

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

/// Whether every edge of a path says which way round the world it goes.
///
/// The unit is the **edge**, not the box around the shape. The ambiguity is a
/// property of a pair of consecutive positions, and a box is only a consequence
/// of them — which is also what lets the refusal name the offending pair, and
/// what keeps a shape that legitimately spans the world: a cap reaching over the
/// pole has a box 360° wide and no edge wider than the gap between two of its
/// own corners.
fn directions_are_stated(positions: &[Snapped]) -> Result<(), Refused> {
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

fn no_repeats(positions: &[Snapped]) -> Result<(), Refused> {
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
fn ring_meets_itself(ring: &[Snapped]) -> Option<(Snapped, Snapped)> {
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
fn hole_sits_in(shell: &[Snapped], hole: &[Snapped]) -> Result<(), Refused> {
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
fn holes_are_separate(one: &[Snapped], other: &[Snapped]) -> Result<(), Refused> {
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
fn first_crossing(one: &[Snapped], other: &[Snapped]) -> Option<Snapped> {
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
fn members_are_separate(members: &[Polygon]) -> Result<(), Refused> {
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
