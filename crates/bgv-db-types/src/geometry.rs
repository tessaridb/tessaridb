//! Shapes on a sphere.
//!
//! # The seven shapes, and why exactly these
//!
//! The set is RFC 7946's — point, line, polygon, and the multi- forms, plus a
//! collection. It is the vocabulary every mapping tool already reads, so a shape
//! stored here can leave without being translated, and one arriving from a client
//! needs no dialect.
//!
//! # A coordinate is a pair, and the order is longitude first
//!
//! RFC 7946 §3.1.1 fixes it: `[longitude, latitude]`. The opposite order is the
//! most common bug in geospatial code and it is silent — a point in Paris becomes
//! a point in the Indian Ocean, which is a perfectly valid place. Stating the
//! order here rather than in a comment somewhere is the only defence a type can
//! offer.
//!
//! Altitude is deliberately not carried. Two dimensions is what an index can
//! cover and what every predicate this store will answer needs; a third would be
//! stored, never queried, and would have to be preserved by every operation that
//! touches a shape.
//!
//! # Equality is on bits, and that is a decision
//!
//! A value in this store lives in ordered sets and hashed maps, so a geometry
//! needs `Eq`, `Hash` and a **total** order — and `f64` offers none of the three.
//!
//! So equality compares bit patterns and ordering uses IEEE-754's total order
//! ([`f64::total_cmp`]). The consequence, stated rather than left to be
//! discovered: `0.0` and `-0.0` are **different** coordinates here, and a NaN
//! coordinate equals itself. Neither arises from a real measurement; both would
//! otherwise make a set of shapes misbehave in a way nothing reports.

use core::cmp::Ordering;
use core::hash::{Hash, Hasher};

/// One position on the sphere: **longitude first**, then latitude, in degrees.
#[derive(Debug, Clone, Copy)]
pub struct Position {
    /// Degrees east of the prime meridian, in `[-180, 180]`.
    pub longitude: f64,
    /// Degrees north of the equator, in `[-90, 90]`.
    pub latitude: f64,
}

impl Position {
    /// A position, longitude first.
    ///
    /// The argument order matches the storage order and RFC 7946, so a reader of
    /// a call site sees the same order as a reader of the bytes.
    #[must_use]
    pub const fn new(longitude: f64, latitude: f64) -> Self {
        Self {
            longitude,
            latitude,
        }
    }

    /// Whether this position is on the sphere at all.
    ///
    /// Not enforced by the constructor: a coordinate arrives from a decoder as
    /// often as from a caller, and a decoder's job is to report what the bytes
    /// said rather than to refuse them. Validation belongs where a shape is
    /// accepted, and this is what that check calls.
    #[must_use]
    pub fn is_on_the_sphere(&self) -> bool {
        (-180.0..=180.0).contains(&self.longitude)
            && (-90.0..=90.0).contains(&self.latitude)
            && self.longitude.is_finite()
            && self.latitude.is_finite()
    }

    const fn keys(&self) -> (u64, u64) {
        (self.longitude.to_bits(), self.latitude.to_bits())
    }
}

impl PartialEq for Position {
    fn eq(&self, other: &Self) -> bool {
        self.keys() == other.keys()
    }
}

impl Eq for Position {}

impl Hash for Position {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.keys().hash(state);
    }
}

impl Ord for Position {
    fn cmp(&self, other: &Self) -> Ordering {
        self.longitude
            .total_cmp(&other.longitude)
            .then_with(|| self.latitude.total_cmp(&other.latitude))
    }
}

impl PartialOrd for Position {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A closed ring of positions, as a polygon's boundary.
///
/// The first and last position are the same one in RFC 7946. That is **not**
/// enforced on construction, for the same reason a position's range is not: a
/// decoder reports what it read. [`Ring::is_closed`] is what an acceptance check
/// calls.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ring(pub Vec<Position>);

impl Ring {
    /// Whether the ring closes, which a polygon boundary must.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        match (self.0.first(), self.0.last()) {
            // A ring needs at least four positions to bound any area: three
            // corners and the repeat that closes it.
            (Some(first), Some(last)) => self.0.len() >= 4 && first == last,
            _ => false,
        }
    }
}

/// A polygon: an outer ring, then any number of holes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Polygon {
    /// The boundary.
    pub exterior: Ring,
    /// Rings cut out of it.
    pub interiors: Vec<Ring>,
}

/// A shape.
/// Not `#[non_exhaustive]`, deliberately: the codec matches on this, and an
/// exhaustive match is what makes adding a shape a compile error at every place
/// that must learn about it rather than a wildcard that swallows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Geometry {
    /// One position.
    Point(Position),
    /// An open path through positions.
    Line(Vec<Position>),
    /// A bounded area, possibly with holes.
    Polygon(Polygon),
    /// Several points as one shape.
    MultiPoint(Vec<Position>),
    /// Several paths as one shape.
    MultiLine(Vec<Vec<Position>>),
    /// Several areas as one shape.
    MultiPolygon(Vec<Polygon>),
    /// Shapes of mixed kinds, as one.
    ///
    /// Boxed because a collection holds geometries and a geometry may be a
    /// collection; without the box the type would have no finite size.
    Collection(Vec<Box<Geometry>>),
}

impl Geometry {
    /// The name this shape carries in an error or a rendering.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Point(_) => "point",
            Self::Line(_) => "line",
            Self::Polygon(_) => "polygon",
            Self::MultiPoint(_) => "multipoint",
            Self::MultiLine(_) => "multiline",
            Self::MultiPolygon(_) => "multipolygon",
            Self::Collection(_) => "collection",
        }
    }

    /// Every position this shape is made of, in storage order.
    ///
    /// What a bounding box, an index cover and a validity check all need, so
    /// each of them asks this rather than walking the variants again. A second
    /// walk that drifted would not fail to compile — it would return the wrong
    /// rows.
    pub fn positions(&self) -> Vec<Position> {
        let mut out = Vec::new();
        self.collect_positions(&mut out);
        out
    }

    fn collect_positions(&self, into: &mut Vec<Position>) {
        match self {
            Self::Point(position) => into.push(*position),
            Self::Line(positions) | Self::MultiPoint(positions) => {
                into.extend_from_slice(positions);
            }
            Self::Polygon(polygon) => collect_polygon(polygon, into),
            Self::MultiLine(lines) => {
                for line in lines {
                    into.extend_from_slice(line);
                }
            }
            Self::MultiPolygon(polygons) => {
                for polygon in polygons {
                    collect_polygon(polygon, into);
                }
            }
            Self::Collection(shapes) => {
                for shape in shapes {
                    shape.collect_positions(into);
                }
            }
        }
    }

    /// Whether every position is on the sphere and every ring closes.
    ///
    /// An acceptance check, not an invariant of the type — see [`Position::is_on_the_sphere`].
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        if !self.positions().iter().all(Position::is_on_the_sphere) {
            return false;
        }
        match self {
            Self::Polygon(polygon) => polygon_is_closed(polygon),
            Self::MultiPolygon(polygons) => polygons.iter().all(polygon_is_closed),
            Self::Collection(shapes) => shapes.iter().all(|shape| shape.is_well_formed()),
            Self::Point(_) | Self::MultiPoint(_) => true,
            // A line needs two distinct ends to be a path at all.
            Self::Line(positions) => positions.len() >= 2,
            Self::MultiLine(lines) => lines.iter().all(|line| line.len() >= 2),
        }
    }
}

fn collect_polygon(polygon: &Polygon, into: &mut Vec<Position>) {
    into.extend_from_slice(&polygon.exterior.0);
    for interior in &polygon.interiors {
        into.extend_from_slice(&interior.0);
    }
}

fn polygon_is_closed(polygon: &Polygon) -> bool {
    polygon.exterior.is_closed() && polygon.interiors.iter().all(Ring::is_closed)
}
