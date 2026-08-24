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

/// A position in **half** grid units — every grid coordinate doubled.
///
/// # Why a second position type exists
///
/// One operation the predicates above cannot express is *the middle of a
/// segment*. The midpoint of two grid points has a half-unit coordinate whenever
/// the two ends differ by an odd number of units, so it is not a [`Snapped`] and
/// cannot be made one without rounding — and rounding a probe point is exactly
/// how a containment test starts answering about a position nobody asked about.
///
/// Doubling the whole coordinate system makes that midpoint an integer again.
/// The sum of two even numbers is even, so `(one + other) / 2` is exact for any
/// pair of doubled grid points, and every predicate below stays an integer
/// comparison with an exact sign.
///
/// **One level, and the type says so.** A midpoint *of midpoints* is a quarter
/// unit and would not be exact here. Nothing in this crate takes that second
/// step, and [`Fine::midway`] is documented as requiring doubled grid points
/// rather than arbitrary `Fine` values.
///
/// Overflow: a doubled coordinate reaches 3.6 × 10^11, a difference 7.2 × 10^11,
/// and a product in an orientation test about 5.2 × 10^23 — fifteen orders below
/// `i128`'s ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Fine {
    longitude: i128,
    latitude: i128,
}

impl Fine {
    /// A grid point in half units.
    pub(crate) fn of(position: Snapped) -> Self {
        Self {
            longitude: i128::from(position.longitude_units()).saturating_mul(2),
            latitude: i128::from(position.latitude_units()).saturating_mul(2),
        }
    }

    /// The position halfway between two **doubled grid points**.
    ///
    /// Exact for that input and only for it, which is why every caller passes
    /// [`Fine::of`] results rather than anything computed.
    pub(crate) fn midway(one: Self, other: Self) -> Self {
        Self {
            longitude: one
                .longitude
                .saturating_add(other.longitude)
                .saturating_div(2),
            latitude: one
                .latitude
                .saturating_add(other.latitude)
                .saturating_div(2),
        }
    }
}

/// Which side of the directed line `from` → `to` the position `of` falls on.
///
/// The sign of the cross product of the two edge vectors, computed in `i128`
/// over grid units. No product can overflow: a coordinate difference is at most
/// 3.6 × 10^11 and a longitude is at most 1.8 × 10^11, so the largest term is
/// about 6.5 × 10^22 against `i128`'s 1.7 × 10^38.
#[must_use]
pub fn orientation(from: Snapped, to: Snapped, of: Snapped) -> Orientation {
    orientation_fine(Fine::of(from), Fine::of(to), Fine::of(of))
}

/// [`orientation`] over half units.
///
/// The single implementation; the grid-unit form above delegates here. A second
/// copy specialised to [`Snapped`] would not fail to compile if the two drifted
/// — it would return different sides for the same question.
pub(crate) fn orientation_fine(from: Fine, to: Fine, of: Fine) -> Orientation {
    let edge_longitude = to.longitude.saturating_sub(from.longitude);
    let edge_latitude = to.latitude.saturating_sub(from.latitude);
    let to_longitude = of.longitude.saturating_sub(from.longitude);
    let to_latitude = of.latitude.saturating_sub(from.latitude);

    // Saturating rather than bare, because the workspace refuses arithmetic that
    // could wrap. Neither product can reach the saturation point: the bound above
    // puts the largest at about 5.2 × 10^23 against `i128`'s 1.7 × 10^38, so
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

/// Whether `of` lies on the closed segment `from` → `to`, endpoints included.
///
/// Collinearity alone is not enough: every position on the infinite line through
/// the segment is collinear with it, including the ones far outside. The second
/// test is the segment's bounding box, which for a collinear position is exactly
/// the condition of lying between the ends.
#[must_use]
pub fn on_segment(from: Snapped, to: Snapped, of: Snapped) -> bool {
    on_segment_fine(Fine::of(from), Fine::of(to), Fine::of(of))
}

/// [`on_segment`] over half units.
pub(crate) fn on_segment_fine(from: Fine, to: Fine, of: Fine) -> bool {
    orientation_fine(from, to, of) == Orientation::Collinear
        && between(of.longitude, from.longitude, to.longitude)
        && between(of.latitude, from.latitude, to.latitude)
}

fn between(value: i128, one_end: i128, other_end: i128) -> bool {
    value >= one_end.min(other_end) && value <= one_end.max(other_end)
}

/// Whether two closed segments cross **transversally** — each strictly through
/// the other's interior, with no endpoint touching and nothing collinear.
///
/// The strict half of [`segments_meet`], and a different question rather than a
/// stricter version of the same one. A shared endpoint and a collinear overlap
/// are both "they meet" and neither is "one passes through the other", so the
/// two answers are needed in different places: a ring self-intersection test
/// wants the loose one, and a containment test wants this one, because a segment
/// that merely *touches* a boundary has not left the region while a segment that
/// crosses it has.
#[must_use]
pub fn segments_cross(
    first_from: Snapped,
    first_to: Snapped,
    second_from: Snapped,
    second_to: Snapped,
) -> bool {
    let a = orientation(first_from, first_to, second_from);
    let b = orientation(first_from, first_to, second_to);
    let c = orientation(second_from, second_to, first_from);
    let d = orientation(second_from, second_to, first_to);

    a != b
        && c != d
        && a != Orientation::Collinear
        && b != Orientation::Collinear
        && c != Orientation::Collinear
        && d != Orientation::Collinear
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
    ring_contains_fine(ring, Fine::of(of))
}

/// [`ring_contains`] for a position in half units.
///
/// The ring stays in grid units and its vertices are doubled as they are read,
/// which keeps the caller from allocating a second copy of a coastline to ask
/// one question about one probe point.
pub(crate) fn ring_contains_fine(ring: &[Snapped], of: Fine) -> Containment {
    if ring.len() < 4 {
        // Fewer than four positions cannot close around any area, so there is no
        // inside to be in. Reported as outside rather than refused: the caller
        // that cares about well-formedness asked at ingest.
        return Containment::Outside;
    }
    for edge in ring.windows(2) {
        let (from, to) = (Fine::of(edge[0]), Fine::of(edge[1]));
        if on_segment_fine(from, to, of) {
            return Containment::Boundary;
        }
    }

    let mut winding = 0_i32;
    for edge in ring.windows(2) {
        let (from, to) = (Fine::of(edge[0]), Fine::of(edge[1]));
        let latitude = of.latitude;
        if from.latitude <= latitude {
            if to.latitude > latitude && orientation_fine(from, to, of) == Orientation::Left {
                winding = winding.saturating_add(1);
            }
        } else if to.latitude <= latitude && orientation_fine(from, to, of) == Orientation::Right {
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
