//! Putting two dimensions in one order, so a box becomes a range scan.
//!
//! # The problem this solves, and the one it does not
//!
//! The substrate underneath this store is an ordered key-value map. It answers
//! one question well — *give me every key between here and there* — and a
//! spatial query is not that question. "Every shape near this point" has two
//! coordinates, and two coordinates have no order.
//!
//! A space-filling curve manufactures one. It visits every cell of a square grid
//! exactly once, and numbering the cells in visit order gives each a single
//! integer. Nearby cells get nearby numbers *most of the time*, so a small region
//! becomes a small number of ranges, and a range is what the substrate answers.
//!
//! That is the whole of what the curve buys, and it is worth being exact about
//! what it does **not** buy: the numbering is not a spatial index by itself, and
//! two cells that are neighbours on the grid can be far apart in the numbering.
//! The curve makes the ranges *few*; it never makes them exact. Which is why the
//! property this module owes is not locality at all — see below.
//!
//! # Why Hilbert rather than the interleave everybody writes first
//!
//! Interleaving the bits of the two coordinates gives a usable ordering in four
//! lines of code. Its cost is the *jump*: the curve leaves a quadrant and returns
//! to it, so a compact region breaks into many more ranges than its area
//! warrants, and every extra range is another seek.
//!
//! The Hilbert curve has no jumps — consecutive numbers are always adjacent
//! cells — which is what keeps a region's range count near its perimeter rather
//! than near its area. It costs a loop instead of a shift, and that is a good
//! trade for a store: the seek is what a range scan pays for, and the loop runs
//! once per key.
//!
//! # The property this module actually owes
//!
//! Locality is what the curve is *for*, but it is an efficiency property: a bad
//! curve is slow. The property that makes the module *correct* is a different
//! one, and it does not involve the curve at all:
//!
//! > Every position inside a box has its cell inside the covering of that box.
//!
//! Break locality and queries get slower. Break completeness and queries return
//! **fewer rows than exist**, with nothing raised anywhere — the failure
//! direction a filter must not have, and the one the geo programme has been
//! organised around since the oracle wave proved it for [`Bounds::meets`].
//!
//! The proof is short, and it deliberately says nothing about Hilbert: the map
//! from grid units to cell coordinates is **monotone**, so a position inside a
//! box maps into the box's own cell-coordinate rectangle; the covering keeps
//! every cell that meets that rectangle; therefore it keeps the position's cell.
//! The curve decides only what order those cells are named in. Anything that
//! reasons about the curve to argue completeness is reasoning about the wrong
//! thing.
//!
//! # Two decisions, stated where they can be argued with
//!
//! **The grid is rescaled onto a square, not projected onto one.** A curve is
//! defined on `2^n × 2^n`; longitude spans twice what latitude does. Both axes
//! are mapped independently onto `[0, 2^ORDER)`, which makes a cell twice as wide
//! as it is tall in degrees. That is not an error being tolerated — the curve is
//! asked for locality and for the range property, never for equal-area cells, and
//! a store that needed equal area would need a spherical cell scheme rather than
//! a rescaled planar one.
//!
//! **[`ORDER`] is 32, so a number is a `u64` and a cell key is eight bytes.** Two
//! axes at 32 bits fill the word exactly. The finest cell is then about
//! `8.4 × 10^-8°` across — roughly **9 mm** of longitude at the equator and 5 mm
//! of latitude, some 84 grid units wide, so still far coarser than the grid
//! itself. Going finer means going wider than a word: one cell per grid unit
//! needs 39 bits an axis, hence a 78-bit number and a 16-byte key on every entry
//! of every spatial index for as long as the store exists, bought to resolve
//! below a tenth of a millimetre.
//!
//! # The two rectangles, and why they are not one
//!
//! A cell is coarser than the grid, so the cell-coordinate rectangle of a box is
//! *larger* than the box: it reaches up to a cell outside it on every side. That
//! rounding has to go **outward** for the test deciding which cells to keep, or
//! the covering loses rows.
//!
//! It has to go **inward** for the test deciding which cells lie wholly inside
//! the box, and using the outward rectangle for both is the trap. It is worth
//! saying exactly where that bites, because "it is obviously wrong" is not true
//! and the imprecise version is why it survives review: a cell strictly inside
//! the rounded rectangle really is strictly inside the box, so the mistake is
//! invisible for every cell a large box is covered by. It goes wrong only when a
//! cell's edge coordinate **equals** the box's rounded edge — which a coarse cell
//! can rarely reach and a fine one reaches constantly. So the mistake is
//! harmless on world-scale boxes and wrong on the small ones a store is actually
//! asked, and a test suite built from large random boxes certifies it.
//!
//! Hence two rectangles: [`Square`] for keeping, and the grid extent recovered by
//! [`Cell::extent`] for the [`Class::Interior`] claim.

mod hilbert;

use crate::bounds::Bounds;
use crate::grid::Snapped;
use hilbert::first_offset;
pub use hilbert::{hilbert_index, hilbert_point};

/// Bits per axis at the finest level.
///
/// Two axes at this width fill a `u64` exactly, which is what keeps a cell key
/// eight bytes.
pub const ORDER: u32 = 32;

/// Cell coordinates per axis: `2^ORDER`.
const SIDE: u64 = 1 << ORDER;

/// The largest cell coordinate on either axis.
const LAST: u64 = SIDE - 1;

/// The full longitude span of the grid, in grid units.
const LONGITUDE_SPAN: i128 = 360_000_000_000;

/// The full latitude span of the grid, in grid units.
const LATITUDE_SPAN: i128 = 180_000_000_000;

/// What a longitude in grid units is shifted by to reach zero.
const LONGITUDE_ORIGIN: i128 = 180_000_000_000;

/// What a latitude in grid units is shifted by to reach zero.
const LATITUDE_ORIGIN: i128 = 90_000_000_000;

/// How a covering cell stands to the box it was produced for.
///
/// Worth a type because it is the difference between work a reader must do and
/// work it may skip: every grid position under an [`Class::Interior`] cell is
/// inside the box, so a candidate found there has passed the box test already,
/// while a [`Class::Boundary`] cell holds positions on both sides of the edge.
///
/// It is deliberately a statement about the **box**, not about the query's shape
/// and not about any record. This layer knows rectangles. Whether a candidate may
/// skip the *predicate* is a question for the layer that knows both the query
/// geometry and the record's own extent, and answering it here would be guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Every grid position under this cell is inside the box.
    Interior,
    /// The cell holds grid positions on both sides of the box's edge.
    Boundary,
}

/// One square of the cell grid, at one level of subdivision.
///
/// Level 0 is the whole world as a single cell; level [`ORDER`] is the finest. A
/// cell's index numbers it among the `4^level` cells of its own level, in curve
/// order — so a cell is exactly a **contiguous range** of finest-level numbers,
/// which is what lets the substrate answer it with one scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cell {
    level: u32,
    index: u64,
}

impl Cell {
    /// The whole world, undivided.
    #[must_use]
    pub const fn root() -> Self {
        Self { level: 0, index: 0 }
    }

    /// The finest cell holding this position.
    #[must_use]
    pub fn containing(position: Snapped) -> Self {
        Self {
            level: ORDER,
            index: hilbert_index(
                longitude_coordinate(position.longitude_units()),
                latitude_coordinate(position.latitude_units()),
            ),
        }
    }

    /// The cell of `level` whose range begins at `first`, if one does.
    ///
    /// The inverse of [`Cell::range`]'s left end, and the only way to read a cell
    /// back out of somewhere it was written. A stored cell keeps the **start of
    /// its range** rather than its index within its level, because that is the
    /// number that orders: an index is small at a coarse level and large at a
    /// fine one, so sorting by index would interleave levels rather than
    /// interleaving space.
    ///
    /// `None` when `level` is finer than [`ORDER`] or when `first` is not a
    /// multiple of the level's span — neither can be produced by [`Cell::range`],
    /// so both mean the bytes did not come from a cell.
    #[must_use]
    pub fn starting_at(level: u32, first: u64) -> Option<Self> {
        if level > ORDER {
            return None;
        }
        let width = 2_u32.saturating_mul(ORDER.saturating_sub(level));
        if width >= u64::BITS {
            // The root, whose range begins at zero and whose index is zero.
            return (first == 0).then_some(Self::root());
        }
        let index = first.checked_shr(width)?;
        (index.checked_shl(width)? == first).then_some(Self { level, index })
    }

    /// Which level of subdivision this cell belongs to.
    #[must_use]
    pub const fn level(self) -> u32 {
        self.level
    }

    /// This cell's number among the cells of its own level.
    #[must_use]
    pub const fn index(self) -> u64 {
        self.index
    }

    /// The finest-level numbers this cell covers, both ends included.
    ///
    /// The range property: an aligned square of side `2^(ORDER-level)` occupies a
    /// contiguous run of `4^(ORDER-level)` finest numbers beginning at a multiple
    /// of that length. It follows from the recursive construction rather than
    /// from any particular case, and it is the reason a cell is one scan.
    #[must_use]
    pub fn range(self) -> (u64, u64) {
        let width = 2_u32.saturating_mul(ORDER.saturating_sub(self.level));
        // The root spans the whole word, and `1 << 64` is not a number. Naming
        // the case costs a line; a saturating shift would silently hand back an
        // empty range for the one cell that covers everything.
        if width >= u64::BITS {
            return (0, u64::MAX);
        }
        let span = 1_u64.checked_shl(width).unwrap_or(1);
        let first = self.index.checked_shl(width).unwrap_or(0);
        (first, first.saturating_add(span.saturating_sub(1)))
    }

    /// The four cells this one divides into, in curve order.
    ///
    /// `None` at the finest level, where there is nothing left to divide.
    #[must_use]
    pub fn children(self) -> Option<[Self; 4]> {
        if self.level >= ORDER {
            return None;
        }
        let level = self.level.saturating_add(1);
        let base = self.index.saturating_mul(4);
        Some([
            Self { level, index: base },
            Self {
                level,
                index: base.saturating_add(1),
            },
            Self {
                level,
                index: base.saturating_add(2),
            },
            Self {
                level,
                index: base.saturating_add(3),
            },
        ])
    }

    /// The cell of `level` that holds this one.
    ///
    /// `None` when `level` is finer than this cell's own — a cell has no
    /// ancestors below it, and answering with a descendant instead would hand a
    /// reader a square that does not contain what it asked about.
    ///
    /// A cell's own level is an ancestor of itself, which is what makes a walk
    /// over `0..=level` need no case for the end it starts from.
    ///
    /// This is the query side of the layout in `docs/key-grammar.md` §3c. A
    /// record **larger** than a query box sits at a cell coarser than the query's
    /// own, whose range begins *below* the query cell's range — so a reader that
    /// only scanned forward from the query cell would never reach it and would
    /// answer with fewer rows than exist. There are at most `level` such cells
    /// and each is one truncation, which is why the second half of a spatial
    /// lookup is cheap rather than merely necessary.
    #[must_use]
    pub fn ancestor(self, level: u32) -> Option<Self> {
        if level > self.level {
            return None;
        }
        // Two bits per level of subdivision: each step up merges four cells into
        // one, and the curve numbers children consecutively inside their parent.
        let width = 2_u32.saturating_mul(self.level.saturating_sub(level));
        let index = self.index.checked_shr(width).unwrap_or(0);
        Some(Self { level, index })
    }

    /// Every grid position this cell holds, as a box.
    ///
    /// The inward rectangle, recovered by inverting the placement rather than by
    /// re-rounding the query's box. This is what makes [`Class::Interior`] a
    /// claim about positions instead of a claim about cell arithmetic.
    ///
    /// `None` would mean the inverse placement had left the grid, which cannot
    /// happen — both corners are derived from the grid's own limits — and is
    /// returned rather than asserted so that the impossible case has somewhere to
    /// go other than a panic in a database.
    #[must_use]
    pub fn extent(self) -> Option<Bounds> {
        let square = self.square();
        let west = i64::try_from(longitude_units_from(square.west)).ok()?;
        let south = i64::try_from(latitude_units_from(square.south)).ok()?;
        let east = i64::try_from(longitude_units_upto(square.east)).ok()?;
        let north = i64::try_from(latitude_units_upto(square.north)).ok()?;
        let low = Snapped::from_units(west, south).ok()?;
        let high = Snapped::from_units(east, north).ok()?;
        Some(Bounds::of_position(low).widened_to(high))
    }

    /// The cell-coordinate rectangle this cell occupies.
    fn square(self) -> Square {
        let (first, _) = self.range();
        let (x, y) = hilbert_point(first);
        let width = ORDER.saturating_sub(self.level);
        let side = 1_u64.checked_shl(width).unwrap_or(SIDE);
        // The curve enters an aligned square at one of its corners, and which
        // corner depends on the orientation at that level — so the entry point
        // is not always the low corner and must not be used as one.
        let edge = side.saturating_sub(1);
        let low_x = x & !edge;
        let low_y = y & !edge;
        Square {
            west: low_x,
            south: low_y,
            east: low_x.saturating_add(edge),
            north: low_y.saturating_add(edge),
        }
    }
}

/// A rectangle in cell coordinates, both ends included.
///
/// Deliberately not [`Bounds`], which is in grid units. Mixing the two is the
/// mistake this separate type exists to make impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Square {
    west: u64,
    south: u64,
    east: u64,
    north: u64,
}

impl Square {
    const fn meets(self, other: Self) -> bool {
        self.west <= other.east
            && other.west <= self.east
            && self.south <= other.north
            && other.south <= self.north
    }
}

/// The cell-coordinate rectangle a box occupies, rounded **outward**.
///
/// Monotone in both axes, which is the whole of the completeness argument: a
/// position inside the box has cell coordinates inside this rectangle.
fn square_of(bounds: Bounds) -> Square {
    Square {
        west: longitude_coordinate(bounds.west()),
        south: latitude_coordinate(bounds.south()),
        east: longitude_coordinate(bounds.east()),
        north: latitude_coordinate(bounds.north()),
    }
}

/// Cover a box with cells, refining only where its edge falls.
///
/// The result is complete by construction — every position inside `bounds` has
/// its finest cell inside one of the returned cells' ranges — and it stays
/// complete at every `budget`, because exhausting the budget keeps a **coarser**
/// cell rather than dropping a finer one. A budget of zero is read as one: the
/// world is always coverable.
///
/// The budget is a target rather than a bound, and refinement stops before the
/// step that would pass it. Stopping part-way through a step would return a set
/// that is still complete but whose shape depends on the order the children
/// happened to be visited in, which is not a thing to build a query plan on.
#[must_use]
pub fn covering(bounds: Bounds, budget: usize) -> Vec<(Cell, Class)> {
    let target = budget.max(1);
    let wanted = square_of(bounds);

    let mut settled: Vec<(Cell, Class)> = Vec::new();
    let mut open: Vec<Cell> = vec![Cell::root()];

    loop {
        let mut divisible: Vec<Cell> = Vec::new();
        for cell in open.drain(..) {
            if !cell.square().meets(wanted) {
                continue;
            }
            if cell.extent().is_some_and(|extent| bounds.holds(extent)) {
                settled.push((cell, Class::Interior));
                continue;
            }
            match cell.children() {
                Some(_) => divisible.push(cell),
                None => settled.push((cell, Class::Boundary)),
            }
        }
        if divisible.is_empty() {
            break;
        }
        let after = settled
            .len()
            .saturating_add(divisible.len().saturating_mul(4));
        if after > target {
            settled.extend(divisible.into_iter().map(|cell| (cell, Class::Boundary)));
            break;
        }
        for cell in divisible {
            if let Some(children) = cell.children() {
                open.extend_from_slice(&children);
            }
        }
    }

    settled
}

/// A longitude in grid units, placed on the cell grid.
///
/// Monotone non-decreasing, which is the property completeness rests on.
fn longitude_coordinate(units: i64) -> u64 {
    place(
        i128::from(units).saturating_add(LONGITUDE_ORIGIN),
        LONGITUDE_SPAN,
    )
}

/// A latitude in grid units, placed on the cell grid.
fn latitude_coordinate(units: i64) -> u64 {
    place(
        i128::from(units).saturating_add(LATITUDE_ORIGIN),
        LATITUDE_SPAN,
    )
}

/// `offset / span` of the way across the axis, in cell coordinates.
///
/// Floor division, which is what makes the map monotone: a larger offset never
/// produces a smaller coordinate. Clamped at both ends rather than trusted,
/// because a coordinate one past the end would wrap the number into the opposite
/// corner of the world, and the position that did it would then be findable by
/// no query at all.
fn place(offset: i128, span: i128) -> u64 {
    let clamped = offset.clamp(0, span);
    let scaled = clamped
        .saturating_mul(i128::from(LAST))
        .checked_div(span)
        .unwrap_or(0);
    u64::try_from(scaled.clamp(0, i128::from(LAST))).unwrap_or(LAST)
}

/// The first grid unit that places at this longitude coordinate.
fn longitude_units_from(coordinate: u64) -> i128 {
    first_offset(coordinate, LONGITUDE_SPAN).saturating_sub(LONGITUDE_ORIGIN)
}

/// The first grid unit that places at this latitude coordinate.
fn latitude_units_from(coordinate: u64) -> i128 {
    first_offset(coordinate, LATITUDE_SPAN).saturating_sub(LATITUDE_ORIGIN)
}

/// The last grid unit that places at this longitude coordinate.
fn longitude_units_upto(coordinate: u64) -> i128 {
    if coordinate >= LAST {
        return LONGITUDE_SPAN.saturating_sub(LONGITUDE_ORIGIN);
    }
    longitude_units_from(coordinate.saturating_add(1)).saturating_sub(1)
}

/// The last grid unit that places at this latitude coordinate.
fn latitude_units_upto(coordinate: u64) -> i128 {
    if coordinate >= LAST {
        return LATITUDE_SPAN.saturating_sub(LATITUDE_ORIGIN);
    }
    latitude_units_from(coordinate.saturating_add(1)).saturating_sub(1)
}

#[cfg(test)]
mod tests;
