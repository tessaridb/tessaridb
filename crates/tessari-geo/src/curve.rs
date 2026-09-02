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

use crate::bounds::Bounds;
use crate::grid::Snapped;

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

/// The smallest offset whose placement reaches this coordinate.
///
/// The inverse of [`place`]'s floor: `place(o) >= c` exactly when
/// `o >= ceil(c * span / LAST)`, so that ceiling is the first offset of the
/// coordinate's own run.
fn first_offset(coordinate: u64, span: i128) -> i128 {
    let divisor = i128::from(LAST);
    let numerator = i128::from(coordinate).saturating_mul(span);
    let rounded = numerator
        .saturating_add(divisor.saturating_sub(1))
        .checked_div(divisor)
        .unwrap_or(0);
    rounded.clamp(0, span)
}

/// The curve's number for a cell, at the finest level.
///
/// The standard iterative construction: walk the levels from coarse to fine, read
/// off which quadrant the point is in at each one, then rotate the frame so the
/// next level's quadrants are numbered relative to the direction the curve is
/// travelling. The rotation is the entire difference from a bit interleave, and
/// it is what removes the jumps.
#[must_use]
pub fn hilbert_index(x: u64, y: u64) -> u64 {
    let mut x = x.min(LAST);
    let mut y = y.min(LAST);
    let mut number = 0_u64;
    let mut step = SIDE.wrapping_shr(1);
    while step > 0 {
        let rx = u64::from((x & step) > 0);
        let ry = u64::from((y & step) > 0);
        let quadrant = 3_u64.saturating_mul(rx) ^ ry;
        number = number.saturating_add(step.saturating_mul(step).saturating_mul(quadrant));
        rotate(step, &mut x, &mut y, rx, ry);
        step = step.wrapping_shr(1);
    }
    number
}

/// The cell a curve number names, at the finest level.
///
/// The inverse of [`hilbert_index`]: the same construction run backwards, fine to
/// coarse, undoing each rotation as it goes.
#[must_use]
pub fn hilbert_point(number: u64) -> (u64, u64) {
    let mut remaining = number;
    let mut x = 0_u64;
    let mut y = 0_u64;
    let mut step = 1_u64;
    while step < SIDE {
        let rx = 1 & remaining.wrapping_shr(1);
        let ry = 1 & (remaining ^ rx);
        rotate(step, &mut x, &mut y, rx, ry);
        x = x.saturating_add(step.saturating_mul(rx));
        y = y.saturating_add(step.saturating_mul(ry));
        remaining = remaining.wrapping_shr(2);
        step = step.wrapping_shl(1);
    }
    (x, y)
}

/// Reflect the frame so the next level is read in the curve's own direction.
///
/// Only the two lower quadrants need it, and one of those also needs the axes
/// swapped. Getting this wrong does not produce a broken curve — it produces a
/// *different* traversal that is still a bijection, which is why the inverse test
/// cannot catch it and the adjacency test is the one that does.
fn rotate(step: u64, x: &mut u64, y: &mut u64, rx: u64, ry: u64) {
    if ry != 0 {
        return;
    }
    if rx == 1 {
        let edge = step.saturating_sub(1);
        *x = edge.saturating_sub(*x & edge);
        *y = edge.saturating_sub(*y & edge);
    }
    core::mem::swap(x, y);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn at(longitude: i64, latitude: i64) -> Snapped {
        Snapped::from_units(longitude, latitude).expect("built from the grid's own limits")
    }

    /// A deterministic sequence. These tests need spread rather than randomness,
    /// and a seeded generator keeps a failure reproducible from its own output.
    struct Spread(u64);

    impl Spread {
        const fn from(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            let mut state = self.0;
            state ^= state.wrapping_shl(13);
            state ^= state.wrapping_shr(7);
            state ^= state.wrapping_shl(17);
            self.0 = state;
            state
        }

        /// A value in `[0, bound]`, both ends reachable.
        fn upto(&mut self, bound: u64) -> u64 {
            self.next()
                .checked_rem(bound.saturating_add(1))
                .unwrap_or(0)
        }

        fn longitude(&mut self) -> i64 {
            i64::try_from(self.upto(360_000_000_000))
                .unwrap_or(0)
                .saturating_sub(180_000_000_000)
        }

        fn latitude(&mut self) -> i64 {
            i64::try_from(self.upto(180_000_000_000))
                .unwrap_or(0)
                .saturating_sub(90_000_000_000)
        }

        fn position(&mut self) -> Snapped {
            at(self.longitude(), self.latitude())
        }

        /// A box whose size is drawn from each scale that behaves differently:
        /// a single position, a box narrower than one cell, one a few cells
        /// wide, and one spanning continents.
        ///
        /// The mix is not decoration, and it is not a guess about the workload.
        /// A generator producing only world-scale boxes never lands a box edge
        /// on a cell boundary — the alignment is what a coarse cell cannot
        /// reach — so the two tests written to catch an edge-alignment mistake
        /// both passed over a real one until this method existed. Small boxes
        /// are also what a store is actually asked, which is the lesser reason.
        fn box_of(&mut self) -> Bounds {
            let reach = match self.upto(3) {
                0 => 0,
                1 => 40,
                2 => 5_000,
                _ => 50_000_000_000,
            };
            let anchor = self.position();
            let corner = at(
                anchor
                    .longitude_units()
                    .saturating_add(i64::try_from(self.upto(reach)).unwrap_or(0))
                    .clamp(-180_000_000_000, 180_000_000_000),
                anchor
                    .latitude_units()
                    .saturating_add(i64::try_from(self.upto(reach)).unwrap_or(0))
                    .clamp(-90_000_000_000, 90_000_000_000),
            );
            Bounds::of_position(anchor).widened_to(corner)
        }

        /// A position inside the box, both edges reachable.
        fn inside(&mut self, bounds: Bounds) -> Snapped {
            let width = bounds.east().abs_diff(bounds.west());
            let height = bounds.north().abs_diff(bounds.south());
            at(
                bounds
                    .west()
                    .saturating_add(i64::try_from(self.upto(width)).unwrap_or(0)),
                bounds
                    .south()
                    .saturating_add(i64::try_from(self.upto(height)).unwrap_or(0)),
            )
        }
    }

    /// Whether the covering holds this position's finest cell.
    fn covering_holds(cover: &[(Cell, Class)], position: Snapped) -> bool {
        let number = Cell::containing(position).index();
        cover.iter().any(|(cell, _)| {
            let (first, last) = cell.range();
            number >= first && number <= last
        })
    }

    fn corners(bounds: Bounds) -> [Snapped; 4] {
        [
            at(bounds.west(), bounds.south()),
            at(bounds.west(), bounds.north()),
            at(bounds.east(), bounds.south()),
            at(bounds.east(), bounds.north()),
        ]
    }

    // C1 — the two directions of the curve.

    #[test]
    fn the_curve_and_its_inverse_undo_each_other() {
        let mut spread = Spread::from(0x5eed_1234);
        for _ in 0..2_000 {
            let x = spread.upto(LAST);
            let y = spread.upto(LAST);
            let (back_x, back_y) = hilbert_point(hilbert_index(x, y));
            assert_eq!(
                (x, y),
                (back_x, back_y),
                "the curve numbered ({x}, {y}) and the inverse named another cell"
            );
        }
    }

    #[test]
    fn the_curve_numbers_every_cell_of_a_square_exactly_once() {
        // A full sweep is affordable only over a coarse level, and a bijection
        // over one level of the recursion is a bijection over all of them: the
        // construction is identical at every step. The square is scaled up onto
        // real aligned cell boundaries, so it is the shipped ORDER being tested
        // rather than a scaled-down stand-in for it.
        let side = 64_u64;
        let factor = SIDE.checked_div(side).unwrap_or(1);
        let mut seen: HashSet<u64> = HashSet::new();
        for x in 0..side {
            for y in 0..side {
                assert!(
                    seen.insert(hilbert_index(
                        x.saturating_mul(factor),
                        y.saturating_mul(factor)
                    )),
                    "two cells of the square share a curve number"
                );
            }
        }
        assert_eq!(
            seen.len(),
            usize::try_from(side.saturating_mul(side)).unwrap_or(0),
            "the sweep did not visit every cell"
        );
    }

    // C5 — locality, which is what choosing this curve was for.

    #[test]
    fn consecutive_numbers_are_always_adjacent_cells() {
        let side = 128_u64;
        let factor = SIDE.checked_div(side).unwrap_or(1);
        let block = factor.saturating_mul(factor);
        let mut previous: Option<(u64, u64)> = None;
        for step in 0..side.saturating_mul(side) {
            let (x, y) = hilbert_point(step.saturating_mul(block));
            let here = (
                x.checked_div(factor).unwrap_or(0),
                y.checked_div(factor).unwrap_or(0),
            );
            if let Some(before) = previous {
                let apart = here
                    .0
                    .abs_diff(before.0)
                    .saturating_add(here.1.abs_diff(before.1));
                assert_eq!(
                    apart, 1,
                    "{before:?} and {here:?} are consecutive on the curve and are \
                     not neighbours — the curve jumps, which is the one thing \
                     choosing it over a bit interleave was meant to buy"
                );
            }
            previous = Some(here);
        }
    }

    // C2 — the placement, and its inverse.

    #[test]
    fn placing_a_position_on_the_cell_grid_never_runs_backwards() {
        let mut spread = Spread::from(0x0abc_d001);
        for _ in 0..4_000 {
            let one = spread.longitude();
            let other = spread.longitude();
            let (low, high) = (one.min(other), one.max(other));
            assert!(
                longitude_coordinate(low) <= longitude_coordinate(high),
                "a longitude of {low} placed after {high}; the map is not monotone \
                 and the completeness argument rests on nothing"
            );
            let one = spread.latitude();
            let other = spread.latitude();
            let (low, high) = (one.min(other), one.max(other));
            assert!(
                latitude_coordinate(low) <= latitude_coordinate(high),
                "a latitude of {low} placed after {high}; the map is not monotone"
            );
        }
    }

    #[test]
    fn the_ends_of_the_world_land_on_the_ends_of_the_cell_grid() {
        assert_eq!(longitude_coordinate(-180_000_000_000), 0);
        assert_eq!(longitude_coordinate(180_000_000_000), LAST);
        assert_eq!(latitude_coordinate(-90_000_000_000), 0);
        assert_eq!(latitude_coordinate(90_000_000_000), LAST);
    }

    #[test]
    fn the_inverse_placement_names_exactly_the_run_the_placement_produces() {
        // What `Cell::extent` stands on, and therefore what makes
        // `Class::Interior` a claim about positions. Checked from the
        // placement's own side rather than by algebra: both ends of a
        // coordinate's run must place at it, and the unit before the run must
        // place lower.
        let mut spread = Spread::from(0x1e5e_0011);
        for _ in 0..2_000 {
            let coordinate = spread.upto(LAST).max(1);
            let first = i64::try_from(longitude_units_from(coordinate)).unwrap_or(0);
            let last = i64::try_from(longitude_units_upto(coordinate)).unwrap_or(0);
            assert_eq!(
                longitude_coordinate(first),
                coordinate,
                "the first unit of coordinate {coordinate}'s run does not place there"
            );
            assert_eq!(
                longitude_coordinate(last),
                coordinate,
                "the last unit of coordinate {coordinate}'s run does not place there"
            );
            assert!(
                longitude_coordinate(first.saturating_sub(1)) < coordinate,
                "the unit before coordinate {coordinate}'s run places inside it"
            );
        }
    }

    // C3 — the range property.

    #[test]
    fn the_root_covers_every_number_there_is() {
        assert_eq!(Cell::root().range(), (0, u64::MAX));
    }

    #[test]
    fn a_cell_is_a_contiguous_run_of_the_finest_numbers() {
        for level in 1..=ORDER {
            let cell = Cell { level, index: 1 };
            let (first, last) = cell.range();
            let width = 2_u32.saturating_mul(ORDER.saturating_sub(level));
            let expected = 1_u64.checked_shl(width).unwrap_or(0);
            assert_eq!(
                last.saturating_sub(first).saturating_add(1),
                expected,
                "a level-{level} cell should span {expected} finest numbers"
            );
            assert_eq!(
                first.checked_rem(expected).unwrap_or(1),
                0,
                "a level-{level} cell should begin on a multiple of its own span"
            );
        }
    }

    #[test]
    fn a_cell_is_recovered_from_the_start_of_its_own_range() {
        // What a stored cell has to survive. Every level, and a run of indices
        // at each, because the alignment check is a shift pair and a shift pair
        // is exactly where an off-by-one level would hide.
        let mut spread = Spread::from(0x5ce1_1a70);
        for level in 0..=ORDER {
            for _ in 0..8 {
                // A level holds `4^level` cells, so the index is bounded by the
                // level's own width and not by the span each cell covers — the
                // two are complements and swapping them is how a generator ends
                // up producing values the type cannot hold.
                let bits = 2_u32.saturating_mul(level);
                let index = match 1_u64.checked_shl(bits) {
                    Some(count) => spread.next().checked_rem(count).unwrap_or(0),
                    // Past 2^64 cells every `u64` names one.
                    None => spread.next(),
                };
                let cell = Cell { level, index };
                assert_eq!(
                    Cell::starting_at(level, cell.range().0),
                    Some(cell),
                    "a level-{level} cell should read back from where its range begins"
                );
            }
        }
    }

    #[test]
    fn a_start_that_no_cell_of_that_level_begins_at_is_refused() {
        // The failure this guards is a decoder that accepts any pair and hands
        // back a cell describing a square that was never written.
        assert_eq!(Cell::starting_at(ORDER.saturating_add(1), 0), None);
        // From level 1: level 0 holds one cell, whose range begins at zero, and
        // one below zero is still zero.
        for level in 1..ORDER {
            let cell = Cell { level, index: 1 };
            let (first, _) = cell.range();
            assert_eq!(
                Cell::starting_at(level, first.saturating_sub(1)),
                None,
                "a level-{level} cell does not begin one below its own multiple"
            );
        }
    }

    #[test]
    fn an_ancestor_holds_the_whole_range_of_the_cell_it_is_taken_from() {
        // The property the query side rests on, stated as containment of ranges
        // rather than as arithmetic on indices — because the arithmetic is the
        // thing under test and an assertion written in it would agree with
        // itself. A cell's ancestor at every level above it must cover the
        // cell's whole run, and the run must not be merely touched at one end.
        let mut spread = Spread::from(0x0a17_ce55);
        for _ in 0..64 {
            let position = at(
                i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
                i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
            );
            let cell = Cell::containing(position);
            let (first, last) = cell.range();
            for level in 0..=cell.level() {
                let above = cell.ancestor(level).expect("a level at or above its own");
                assert_eq!(above.level(), level);
                let (low, high) = above.range();
                assert!(
                    low <= first && last <= high,
                    "a level-{level} ancestor should hold the whole run of the cell below it"
                );
            }
        }
    }

    #[test]
    fn a_cell_is_its_own_ancestor_and_has_none_below_it() {
        // Both ends of the walk, named rather than left to a caller's off-by-one:
        // `0..=level` needs the self case to be an ancestor, and asking for a
        // finer level must answer nothing rather than a descendant — a descendant
        // does not contain what was asked about, so returning one would put a
        // square in a reader's hands that holds none of its query.
        let cell = Cell::containing(at(2_350_000_000, 48_850_000_000));
        assert_eq!(cell.ancestor(cell.level()), Some(cell));
        assert_eq!(Cell::root().ancestor(0), Some(Cell::root()));
        assert_eq!(Cell::root().ancestor(1), None);
        assert_eq!(cell.ancestor(ORDER.saturating_add(1)), None);
    }

    #[test]
    fn the_ancestor_of_a_start_is_the_cell_that_start_truncates_to() {
        // The two halves of the layout agreeing: the key stores the start of a
        // cell's range, and the ancestor lookups recover a coarser cell by
        // truncating that start. If `ancestor` and `starting_at` disagreed, the
        // lookups would be built on keys nothing ever wrote — and would answer
        // with nothing, silently.
        let mut spread = Spread::from(0x7ce1_1a90);
        for _ in 0..64 {
            let position = at(
                i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
                i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
            );
            let cell = Cell::containing(position);
            for level in 0..=cell.level() {
                let above = cell.ancestor(level).expect("a level at or above its own");
                assert_eq!(
                    Cell::starting_at(level, above.range().0),
                    Some(above),
                    "an ancestor at level {level} should read back from where its range begins"
                );
            }
        }
    }

    #[test]
    fn every_position_in_a_cells_square_numbers_inside_that_cells_range() {
        // The range property from the other side: not merely that the run has
        // the right length, but that it is the run belonging to that square.
        let mut spread = Spread::from(0x0011_2233);
        for level in [1_u32, 4, 8, 16, 24, 31] {
            let count = 1_u64.checked_shl(2_u32.saturating_mul(level)).unwrap_or(1);
            let cell = Cell {
                level,
                index: spread.upto(count.saturating_sub(1)),
            };
            let (first, last) = cell.range();
            let square = cell.square();
            for _ in 0..200 {
                let x = square
                    .west
                    .saturating_add(spread.upto(square.east.saturating_sub(square.west)));
                let y = square
                    .south
                    .saturating_add(spread.upto(square.north.saturating_sub(square.south)));
                let number = hilbert_index(x, y);
                assert!(
                    number >= first && number <= last,
                    "({x}, {y}) is inside a level-{level} cell's square and its \
                     number {number} is outside that cell's range {first}..={last}"
                );
            }
        }
    }

    // C4 — completeness, the property the whole module owes.

    #[test]
    fn a_covering_holds_the_cell_of_every_position_inside_the_box() {
        let mut spread = Spread::from(0xfeed_face);
        for _ in 0..200 {
            let bounds = spread.box_of();
            let cover = covering(bounds, 32);
            for _ in 0..50 {
                let position = spread.inside(bounds);
                assert!(
                    covering_holds(&cover, position),
                    "{position:?} is inside {bounds:?} and the covering does not \
                     hold its cell — every record there is invisible to every \
                     query over that box, and nothing anywhere raises"
                );
            }
        }
    }

    #[test]
    fn the_corners_of_a_box_are_inside_its_own_covering() {
        // Where a rounding error would hide: one unit, at an edge, and nowhere
        // else. Interior points chosen at random would not find it in a lifetime.
        let mut spread = Spread::from(0x00dd_c0de);
        for _ in 0..400 {
            let bounds = spread.box_of();
            let cover = covering(bounds, 32);
            for position in corners(bounds) {
                assert!(
                    covering_holds(&cover, position),
                    "the corner {position:?} of {bounds:?} is not in its own covering"
                );
            }
        }
    }

    #[test]
    fn completeness_survives_every_budget_including_one() {
        let mut spread = Spread::from(0x0b0d_6e70);
        for budget in [1_usize, 2, 3, 7, 16, 64, 256] {
            for _ in 0..40 {
                let bounds = spread.box_of();
                let cover = covering(bounds, budget);
                assert!(
                    !cover.is_empty(),
                    "a covering of {bounds:?} at budget {budget} is empty; nothing \
                     in that box could ever be found"
                );
                for position in corners(bounds) {
                    assert!(
                        covering_holds(&cover, position),
                        "budget {budget} lost {position:?} from the covering of \
                         {bounds:?} — exhausting a budget must keep a coarser \
                         cell, never drop a finer one"
                    );
                }
            }
        }
    }

    #[test]
    fn the_smallest_budget_returns_the_whole_world() {
        let cover = covering(Bounds::of_position(at(0, 0)), 1);
        assert_eq!(cover.len(), 1);
        assert_eq!(cover[0].0, Cell::root());
    }

    #[test]
    fn a_point_refines_all_the_way_to_the_finest_level() {
        // Otherwise every completeness test above would pass over a single root
        // cell, and the index would scan the world for each of them.
        let cover = covering(Bounds::of_position(at(0, 0)), 64);
        let finest = cover
            .iter()
            .map(|(cell, _)| cell.level())
            .max()
            .unwrap_or(0);
        assert_eq!(
            finest, ORDER,
            "a point's covering stopped at level {finest}; refinement is not \
             running and the ranges would be useless"
        );
    }

    // The two classes, and the claim each of them makes.

    #[test]
    fn both_classes_occur_for_a_box_that_swallows_whole_cells() {
        let bounds = Bounds::of_position(at(-170_000_000_000, -80_000_000_000))
            .widened_to(at(170_000_000_000, 80_000_000_000));
        let cover = covering(bounds, 64);
        assert!(
            cover.iter().any(|(_, class)| *class == Class::Interior),
            "no cell of a near-world box is interior to it; the class is decoration"
        );
        assert!(
            cover.iter().any(|(_, class)| *class == Class::Boundary),
            "no cell of a near-world box straddles its edge"
        );
    }

    #[test]
    fn every_position_under_an_interior_cell_is_inside_the_box() {
        // The claim `Class::Interior` makes, checked against real grid positions
        // rather than trusted from the branch that assigned the class. It is the
        // test that fails when the interior check is made against the
        // outward-rounded rectangle — and it only fails because `box_of` reaches
        // the small scales, since that mistake is invisible on the coarse cells
        // a large box is covered by.
        let mut spread = Spread::from(0x0c1a_5555);
        let mut checked = 0_u32;
        for _ in 0..200 {
            let bounds = spread.box_of();
            for (cell, class) in covering(bounds, 64) {
                if class != Class::Interior {
                    continue;
                }
                let extent = cell.extent();
                assert!(
                    extent.is_some(),
                    "a cell marked interior to {bounds:?} has no grid extent"
                );
                for position in extent.into_iter().flat_map(corners) {
                    checked = checked.saturating_add(1);
                    assert!(
                        bounds.holds_position(position),
                        "{position:?} lies under a cell marked interior to \
                         {bounds:?} and is outside it — a reader trusting the \
                         class would skip the box test and return a wrong row"
                    );
                }
            }
        }
        assert!(
            checked > 0,
            "no interior cell was produced at all; the assertion never ran"
        );
    }
}
