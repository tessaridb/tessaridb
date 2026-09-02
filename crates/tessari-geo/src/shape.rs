//! A shape in grid units — the form a predicate can be exact about.
//!
//! # Why this is a different type from `Geometry`
//!
//! [`tessari_types::Geometry`] is the **document** form: degrees, `f64`, exactly
//! what a caller wrote and what the codec stores. [`Shape`] is the
//! **computational** form: integers on the fixed grid, which is the only form in
//! which an orientation test has an exact sign.
//!
//! Keeping them apart is what makes it impossible to hand un-snapped input to a
//! predicate. There is no `Geometry` → predicate path at all; the conversion
//! runs through [`Shape::of`], which is where the refusal lives. A single type
//! carrying both meanings would leave "is this one snapped?" as a question a
//! reader has to answer from context, and the answer would eventually be wrong
//! somewhere nobody looks.
//!
//! # It is not the same job as `accept`
//!
//! [`crate::accept`] decides whether the store will **hold** a shape: it snaps
//! and then judges rings, holes and self-intersection. This module only puts
//! positions on the grid. A shape that arrives here has usually already been
//! through `accept` — it came out of the store — and one that has not is
//! answered about as written, because a query shape is not stored and asking it
//! to be well formed enough to store would refuse questions nobody needs to
//! refuse.
//!
//! The one thing that is still checked is the grid itself: a longitude of 181 is
//! a mistake upstream, and answering a predicate about it would be answering
//! about a place that does not exist.

use tessari_types::{Geometry, Polygon, Position, Ring};

use crate::bounds::Bounds;
use crate::grid::{OffGrid, Snapped};

/// A closed ring in grid units: first position repeated as the last.
pub type Loop = Vec<Snapped>;

/// A bounded area: one outer ring, and the rings cut out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Area {
    /// The outer boundary.
    pub shell: Loop,
    /// The rings cut out of it.
    pub holes: Vec<Loop>,
}

impl Area {
    /// Put a polygon on the grid.
    ///
    /// # Errors
    ///
    /// Returns [`OffGrid`] when any position is not finite or lies off the
    /// sphere.
    pub fn of(polygon: &Polygon) -> Result<Self, OffGrid> {
        snap_polygon(polygon)
    }

    /// Every ring of this area, shell first.
    pub fn rings(&self) -> impl Iterator<Item = &Loop> {
        core::iter::once(&self.shell).chain(self.holes.iter())
    }
}

/// A shape on the grid.
///
/// The seven forms of RFC 7946, mirroring [`tessari_types::Geometry`] one for
/// one so that a reader comparing the two files does not have to hold a mapping
/// in their head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// One position.
    Point(Snapped),
    /// An open path.
    Line(Vec<Snapped>),
    /// A bounded area.
    Polygon(Area),
    /// Several positions as one shape.
    MultiPoint(Vec<Snapped>),
    /// Several paths as one shape.
    MultiLine(Vec<Vec<Snapped>>),
    /// Several areas as one shape.
    MultiPolygon(Vec<Area>),
    /// Shapes of mixed kinds, as one.
    Collection(Vec<Shape>),
}

impl Shape {
    /// Put a whole geometry on the grid.
    ///
    /// # Errors
    ///
    /// Returns [`OffGrid`] when any position is not finite or lies off the
    /// sphere. Nothing is clamped: a coordinate outside the world is a mistake
    /// somewhere upstream, and answering a question about it would be answering
    /// about a place that is not there.
    pub fn of(geometry: &Geometry) -> Result<Self, OffGrid> {
        Ok(match geometry {
            Geometry::Point(position) => Self::Point(Snapped::of(*position)?),
            Geometry::Line(positions) => Self::Line(snap_all(positions)?),
            Geometry::Polygon(polygon) => Self::Polygon(snap_polygon(polygon)?),
            Geometry::MultiPoint(positions) => Self::MultiPoint(snap_all(positions)?),
            Geometry::MultiLine(lines) => Self::MultiLine(
                lines
                    .iter()
                    .map(|line| snap_all(line))
                    .collect::<Result<_, _>>()?,
            ),
            Geometry::MultiPolygon(polygons) => Self::MultiPolygon(
                polygons
                    .iter()
                    .map(snap_polygon)
                    .collect::<Result<_, _>>()?,
            ),
            Geometry::Collection(members) => Self::Collection(
                members
                    .iter()
                    .map(|member| Self::of(member))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }

    /// The smallest box holding this shape, or `None` when it holds nothing.
    ///
    /// `None` rather than a degenerate box at the origin: an empty shape has no
    /// smallest rectangle, and inventing one would put it off the coast of
    /// Africa where it would answer queries.
    #[must_use]
    pub fn bounds(&self) -> Option<Bounds> {
        let mut held: Option<Bounds> = None;
        self.each_position(&mut |position| {
            held = Some(match held {
                None => Bounds::of_position(position),
                Some(bounds) => bounds.widened_to(position),
            });
        });
        held
    }

    /// Every position, in storage order.
    ///
    /// One walk, used by the box, by the predicates and by anything later that
    /// needs the vertices. A second walk that drifted would not fail to compile
    /// — it would return the wrong rows.
    pub fn each_position(&self, visit: &mut impl FnMut(Snapped)) {
        match self {
            Self::Point(position) => visit(*position),
            Self::Line(positions) | Self::MultiPoint(positions) => {
                for position in positions {
                    visit(*position);
                }
            }
            Self::Polygon(area) => visit_area(area, visit),
            Self::MultiLine(lines) => {
                for line in lines {
                    for position in line {
                        visit(*position);
                    }
                }
            }
            Self::MultiPolygon(areas) => {
                for area in areas {
                    visit_area(area, visit);
                }
            }
            Self::Collection(members) => {
                for member in members {
                    member.each_position(visit);
                }
            }
        }
    }

    /// Every area in this shape, however deeply nested.
    pub fn each_area<'a>(&'a self, visit: &mut impl FnMut(&'a Area)) {
        match self {
            Self::Polygon(area) => visit(area),
            Self::MultiPolygon(areas) => {
                for area in areas {
                    visit(area);
                }
            }
            Self::Collection(members) => {
                for member in members {
                    member.each_area(visit);
                }
            }
            Self::Point(_) | Self::Line(_) | Self::MultiPoint(_) | Self::MultiLine(_) => {}
        }
    }

    /// Every segment of every path and every ring, as ordered pairs.
    ///
    /// Ring edges are segments too. Keeping them in one enumeration is what lets
    /// the predicates ask "do any two boundary pieces meet" once rather than in
    /// four combinations that could each be got wrong separately.
    pub fn each_segment(&self, visit: &mut impl FnMut(Snapped, Snapped)) {
        match self {
            Self::Line(positions) => visit_path(positions, visit),
            Self::MultiLine(lines) => {
                for line in lines {
                    visit_path(line, visit);
                }
            }
            Self::Polygon(area) => visit_area_edges(area, visit),
            Self::MultiPolygon(areas) => {
                for area in areas {
                    visit_area_edges(area, visit);
                }
            }
            Self::Collection(members) => {
                for member in members {
                    member.each_segment(visit);
                }
            }
            Self::Point(_) | Self::MultiPoint(_) => {}
        }
    }

    /// The boundary of this shape, as a shape.
    ///
    /// What separates `contains` from `covers`: a shape lying entirely on
    /// another's boundary is covered by it and not contained in it, and this is
    /// the function that makes that difference sayable rather than a special
    /// case written twice.
    ///
    /// An area's boundary is its rings. A path's boundary is its two ends, and a
    /// **closed** path has none — a ring drawn as a line has no start and no
    /// finish. A position's boundary is empty.
    #[must_use]
    pub fn boundary(&self) -> Self {
        match self {
            Self::Point(_) | Self::MultiPoint(_) => Self::Collection(Vec::new()),
            Self::Line(positions) => path_ends(positions),
            Self::MultiLine(lines) => {
                Self::Collection(lines.iter().map(|line| path_ends(line)).collect())
            }
            Self::Polygon(area) => Self::MultiLine(area.rings().cloned().collect()),
            Self::MultiPolygon(areas) => Self::MultiLine(
                areas
                    .iter()
                    .flat_map(|area| area.rings().cloned())
                    .collect(),
            ),
            Self::Collection(members) => {
                Self::Collection(members.iter().map(Self::boundary).collect())
            }
        }
    }
}

fn path_ends(positions: &[Snapped]) -> Shape {
    match (positions.first(), positions.last()) {
        (Some(first), Some(last)) if first != last => Shape::MultiPoint(vec![*first, *last]),
        _ => Shape::MultiPoint(Vec::new()),
    }
}

fn visit_path(positions: &[Snapped], visit: &mut impl FnMut(Snapped, Snapped)) {
    for edge in positions.windows(2) {
        visit(edge[0], edge[1]);
    }
}

fn visit_area_edges(area: &Area, visit: &mut impl FnMut(Snapped, Snapped)) {
    for ring in area.rings() {
        visit_path(ring, visit);
    }
}

fn visit_area(area: &Area, visit: &mut impl FnMut(Snapped)) {
    for ring in area.rings() {
        for position in ring {
            visit(*position);
        }
    }
}

fn snap_all(positions: &[Position]) -> Result<Vec<Snapped>, OffGrid> {
    positions.iter().copied().map(Snapped::of).collect()
}

fn snap_polygon(polygon: &Polygon) -> Result<Area, OffGrid> {
    Ok(Area {
        shell: snap_ring(&polygon.exterior)?,
        holes: polygon
            .interiors
            .iter()
            .map(snap_ring)
            .collect::<Result<_, _>>()?,
    })
}

fn snap_ring(ring: &Ring) -> Result<Loop, OffGrid> {
    snap_all(&ring.0)
}
