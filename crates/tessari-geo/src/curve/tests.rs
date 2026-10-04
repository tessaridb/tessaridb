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
