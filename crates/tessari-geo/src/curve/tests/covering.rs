use super::*;

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
