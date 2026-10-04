use super::*;

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
