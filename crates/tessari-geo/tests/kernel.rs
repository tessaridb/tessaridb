//! The kernel against an independent oracle and a corpus of awkward shapes.
//!
//! Every wrong geospatial answer looks exactly like a right one — there is no
//! exception, no NaN and no error, just a plausible shape in a plausible place.
//! So nothing here is checked by looking at output and finding it reasonable.
//! Each property is checked against either a **second, differently-derived
//! implementation** or a fixture whose right answer is known before the code
//! runs.

use tessari_geo::predicate::{
    on_segment, orientation, ring_contains, segments_meet, twice_signed_area,
};
use tessari_geo::{Bounds, Containment, Orientation, Snapped};
use tessari_types::Position;

/// A grid point from degrees, for fixtures whose coordinates are exact on the grid.
fn at(longitude: f64, latitude: f64) -> Snapped {
    Snapped::of(Position::new(longitude, latitude)).expect("fixture is on the sphere")
}

/// A closed unit square, counter-clockwise, from (0,0) to (1,1).
fn unit_square() -> Vec<Snapped> {
    vec![
        at(0.0, 0.0),
        at(1.0, 0.0),
        at(1.0, 1.0),
        at(0.0, 1.0),
        at(0.0, 0.0),
    ]
}

// ---------------------------------------------------------------- the grid

#[test]
fn a_position_on_the_grid_round_trips_unchanged() {
    for (longitude, latitude) in [
        (0.0, 0.0),
        (-179.999_999_999, -89.999_999_999),
        (2.294_481_0, 48.858_37),
    ] {
        let snapped = at(longitude, latitude);
        let back = snapped.to_position();
        let again = Snapped::of(back).expect("a snapped position is on the sphere");
        assert_eq!(snapped, again, "{longitude},{latitude} did not round-trip");
    }
}

#[test]
fn a_position_finer_than_the_grid_snaps_once_and_then_never_moves() {
    // This is what "lossless at the declared precision" means: the first write
    // rounds, and every read-modify-write after it is exact.
    let finer = Position::new(1.000_000_000_4, 2.000_000_000_6);
    let first = Snapped::of(finer).expect("on the sphere");
    let second = Snapped::of(first.to_position()).expect("on the sphere");
    assert_eq!(first, second);
    assert_eq!(first.longitude_units(), 1_000_000_000);
    assert_eq!(first.latitude_units(), 2_000_000_001);
}

#[test]
fn a_coordinate_off_the_sphere_is_refused_rather_than_wrapped() {
    // Wrapping 181 to -179 would move a point across the world without saying so.
    assert!(Snapped::of(Position::new(181.0, 0.0)).is_err());
    assert!(Snapped::of(Position::new(0.0, 90.000_1)).is_err());
    assert!(Snapped::of(Position::new(f64::NAN, 0.0)).is_err());
    assert!(Snapped::of(Position::new(0.0, f64::INFINITY)).is_err());
    // The exact limits are on the sphere.
    assert!(Snapped::of(Position::new(180.0, 90.0)).is_ok());
    assert!(Snapped::of(Position::new(-180.0, -90.0)).is_ok());
}

// ---------------------------------------------------------- orientation

#[test]
fn orientation_answers_the_three_cases_and_reverses_with_the_line() {
    let (from, to) = (at(0.0, 0.0), at(1.0, 0.0));
    assert_eq!(orientation(from, to, at(0.5, 1.0)), Orientation::Left);
    assert_eq!(orientation(from, to, at(0.5, -1.0)), Orientation::Right);
    assert_eq!(orientation(from, to, at(0.5, 0.0)), Orientation::Collinear);
    // Collinear beyond the ends is still collinear — that is why `on_segment`
    // needs a second test.
    assert_eq!(orientation(from, to, at(9.0, 0.0)), Orientation::Collinear);

    // Reversing the line reverses every side.
    assert_eq!(orientation(to, from, at(0.5, 1.0)), Orientation::Right);
    assert_eq!(orientation(to, from, at(0.5, -1.0)), Orientation::Left);
}

#[test]
fn one_grid_unit_off_the_line_is_off_the_line() {
    // The exactness claim, made checkable: a deviation of a single unit — the
    // smallest difference the store can represent at all — is reported as a
    // side, not folded into collinear by a tolerance. There is no tolerance.
    let from = Snapped::from_units(0, 0).expect("on the sphere");
    let to = Snapped::from_units(180_000_000_000, 0).expect("on the sphere");
    let dead_on = Snapped::from_units(90_000_000_000, 0).expect("on the sphere");
    let one_above = Snapped::from_units(90_000_000_000, 1).expect("on the sphere");
    let one_below = Snapped::from_units(90_000_000_000, -1).expect("on the sphere");

    assert_eq!(orientation(from, to, dead_on), Orientation::Collinear);
    assert_eq!(orientation(from, to, one_above), Orientation::Left);
    assert_eq!(orientation(from, to, one_below), Orientation::Right);
}

#[test]
fn orientation_is_consistent_however_the_three_are_ordered() {
    // The property whose failure makes clipping and triangulation go wrong: a
    // swap of two arguments must flip the side, every time, for every triple.
    let corpus = [
        (at(0.0, 0.0), at(3.0, 1.0), at(1.0, 2.0)),
        (at(-179.0, -89.0), at(179.0, 89.0), at(0.0, 0.5)),
        (at(10.0, 10.0), at(20.0, 20.0), at(15.0, 15.0)),
    ];
    for (a, b, c) in corpus {
        let flip = |side| match side {
            Orientation::Left => Orientation::Right,
            Orientation::Right => Orientation::Left,
            Orientation::Collinear => Orientation::Collinear,
        };
        assert_eq!(orientation(b, a, c), flip(orientation(a, b, c)));
        assert_eq!(orientation(a, c, b), flip(orientation(a, b, c)));
        // Rotation preserves it.
        assert_eq!(orientation(b, c, a), orientation(a, b, c));
        assert_eq!(orientation(c, a, b), orientation(a, b, c));
    }
}

// ------------------------------------------------------------- segments

#[test]
fn a_position_is_on_a_segment_only_between_its_ends() {
    let (from, to) = (at(0.0, 0.0), at(2.0, 2.0));
    assert!(on_segment(from, to, at(1.0, 1.0)));
    assert!(on_segment(from, to, from));
    assert!(on_segment(from, to, to));
    assert!(
        !on_segment(from, to, at(3.0, 3.0)),
        "collinear is not enough"
    );
    assert!(!on_segment(from, to, at(1.0, 1.5)));
}

#[test]
fn segments_that_touch_or_overlap_count_as_meeting() {
    // A shared endpoint and a shared stretch are both "they meet". A predicate
    // that said otherwise would report two edges of one ring as disjoint.
    let crossing = segments_meet(at(0.0, 0.0), at(2.0, 2.0), at(0.0, 2.0), at(2.0, 0.0));
    assert!(crossing, "an ordinary crossing");

    let touching = segments_meet(at(0.0, 0.0), at(1.0, 1.0), at(1.0, 1.0), at(2.0, 0.0));
    assert!(touching, "meeting at a shared endpoint");

    let overlapping = segments_meet(at(0.0, 0.0), at(2.0, 0.0), at(1.0, 0.0), at(3.0, 0.0));
    assert!(overlapping, "collinear and overlapping");

    let collinear_apart = segments_meet(at(0.0, 0.0), at(1.0, 0.0), at(2.0, 0.0), at(3.0, 0.0));
    assert!(!collinear_apart, "collinear but disjoint");

    let parallel = segments_meet(at(0.0, 0.0), at(2.0, 0.0), at(0.0, 1.0), at(2.0, 1.0));
    assert!(!parallel);

    let apart = segments_meet(at(0.0, 0.0), at(1.0, 1.0), at(5.0, 5.0), at(6.0, 6.0));
    assert!(!apart);
}

// ------------------------------------------------------------ the ring

/// An independent point-in-ring answer, by even-odd ray casting.
///
/// Derived differently from the implementation on purpose: that one counts
/// signed windings using the orientation determinant, this one casts a ray east
/// and counts crossings by comparing the crossing's longitude against the
/// point's. For a simple ring the two must agree, and if they ever disagree one
/// of them is wrong — which is the only way this code is ever known to be right.
fn crossings_say_inside(ring: &[Snapped], of: Snapped) -> bool {
    let mut inside = false;
    for edge in ring.windows(2) {
        let (a, b) = (edge[0], edge[1]);
        let (a_lat, b_lat) = (
            i128::from(a.latitude_units()),
            i128::from(b.latitude_units()),
        );
        let (a_lon, b_lon) = (
            i128::from(a.longitude_units()),
            i128::from(b.longitude_units()),
        );
        let (p_lat, p_lon) = (
            i128::from(of.latitude_units()),
            i128::from(of.longitude_units()),
        );
        if (a_lat > p_lat) == (b_lat > p_lat) {
            continue;
        }
        // Crossing longitude is a_lon + (p_lat - a_lat) * (b_lon - a_lon) / (b_lat - a_lat).
        // Compared without dividing, so the comparison stays exact.
        // Saturating for the same reason the kernel is: the workspace refuses
        // arithmetic that could wrap, and these products are bounded far below
        // `i128`'s range. The oracle stays exact.
        let run = b_lat.saturating_sub(a_lat);
        let left = p_lat
            .saturating_sub(a_lat)
            .saturating_mul(b_lon.saturating_sub(a_lon));
        let right = p_lon.saturating_sub(a_lon).saturating_mul(run);
        let strictly_east = if run > 0 { left > right } else { left < right };
        if strictly_east {
            inside = !inside;
        }
    }
    inside
}

#[test]
fn the_winding_answer_agrees_with_an_independently_derived_one() {
    let ring = unit_square();
    // A sweep rather than a handful of points: every tenth of a degree across a
    // region wider than the square, so the outside, the inside and the run
    // straight through two edges are all covered.
    let mut checked = 0_u32;
    for longitude_tenths in -5..=15 {
        for latitude_tenths in -5..=15 {
            let point = at(
                f64::from(longitude_tenths) / 10.0,
                f64::from(latitude_tenths) / 10.0,
            );
            let found = ring_contains(&ring, point);
            if found == Containment::Boundary {
                // The oracle has no third answer, so boundary positions are
                // outside its remit rather than a disagreement.
                continue;
            }
            assert_eq!(
                found == Containment::Inside,
                crossings_say_inside(&ring, point),
                "the two methods disagree at {point:?}"
            );
            checked = checked.saturating_add(1);
        }
    }
    assert!(checked > 300, "the sweep covered only {checked} positions");
}

#[test]
fn the_boundary_is_its_own_answer_and_not_rounded_into_either_side() {
    // `contains` excludes a boundary position where `covers` includes it, so a
    // kernel that folded the boundary into inside or outside could not express
    // both DE-9IM predicates.
    let ring = unit_square();
    assert_eq!(ring_contains(&ring, at(0.5, 0.5)), Containment::Inside);
    assert_eq!(ring_contains(&ring, at(2.0, 0.5)), Containment::Outside);
    assert_eq!(
        ring_contains(&ring, at(0.5, 0.0)),
        Containment::Boundary,
        "on an edge"
    );
    assert_eq!(
        ring_contains(&ring, at(0.0, 0.0)),
        Containment::Boundary,
        "on a vertex"
    );
    assert_eq!(
        ring_contains(&ring, at(1.0, 1.0)),
        Containment::Boundary,
        "on the far vertex"
    );
}

#[test]
fn a_ring_with_a_notch_excludes_the_notch() {
    // A concave shape, because a convex fixture cannot tell a correct
    // point-in-polygon test from several incorrect ones.
    let ring = vec![
        at(0.0, 0.0),
        at(4.0, 0.0),
        at(4.0, 4.0),
        at(2.0, 4.0),
        at(2.0, 1.0),
        at(0.0, 1.0),
        at(0.0, 0.0),
    ];
    assert_eq!(ring_contains(&ring, at(1.0, 0.5)), Containment::Inside);
    assert_eq!(ring_contains(&ring, at(3.0, 3.0)), Containment::Inside);
    assert_eq!(
        ring_contains(&ring, at(1.0, 3.0)),
        Containment::Outside,
        "inside the notch"
    );
    for longitude_tenths in 0..=40 {
        for latitude_tenths in 0..=40 {
            let point = at(
                f64::from(longitude_tenths) / 10.0,
                f64::from(latitude_tenths) / 10.0,
            );
            if ring_contains(&ring, point) == Containment::Boundary {
                continue;
            }
            assert_eq!(
                ring_contains(&ring, point) == Containment::Inside,
                crossings_say_inside(&ring, point),
                "the two methods disagree at {point:?}"
            );
        }
    }
}

#[test]
fn the_signed_area_reports_the_rings_direction() {
    let counter_clockwise = unit_square();
    let mut clockwise = counter_clockwise.clone();
    clockwise.reverse();
    // One square degree, doubled, in squared grid units.
    assert_eq!(
        twice_signed_area(&counter_clockwise),
        2_000_000_000_000_000_000
    );
    assert_eq!(twice_signed_area(&clockwise), -2_000_000_000_000_000_000);
}

// ------------------------------------------------------------- the box

#[test]
fn the_box_holds_every_position_it_was_built_from() {
    let positions = unit_square();
    let bounds = Bounds::of_positions(&positions).expect("a non-empty ring has a box");
    for position in &positions {
        assert!(bounds.holds_position(*position));
    }
    assert_eq!(bounds.west(), 0);
    assert_eq!(bounds.south(), 0);
    assert_eq!(bounds.east(), 1_000_000_000);
    assert_eq!(bounds.north(), 1_000_000_000);
    assert!(Bounds::of_positions(&[]).is_none(), "nothing has no box");
}

#[test]
fn boxes_that_touch_along_an_edge_meet() {
    // Closed rather than open: a shape lying exactly on a query's border is a
    // candidate the refine step must get the chance to judge.
    let left = Bounds::of_positions(&[at(0.0, 0.0), at(1.0, 1.0)]).expect("non-empty");
    let right = Bounds::of_positions(&[at(1.0, 0.0), at(2.0, 1.0)]).expect("non-empty");
    let apart = Bounds::of_positions(&[at(3.0, 0.0), at(4.0, 1.0)]).expect("non-empty");
    assert!(left.meets(right));
    assert!(!left.meets(apart));
    assert!(left.union(right).holds(left));
    assert!(left.union(right).holds(right));
}

#[test]
fn a_box_around_an_unsplit_antimeridian_crossing_is_recognisable() {
    // A shape from 179°E to 179°W is a few kilometres wide. A box around its
    // raw positions runs the other way round the planet, so the row becomes a
    // candidate for every query in the store and refinement rejects it every
    // time — the index is silently useless for exactly that row.
    let wrapped = Bounds::of_positions(&[at(179.0, 0.0), at(-179.0, 1.0)]).expect("non-empty");
    assert!(wrapped.spans_more_than_half_the_world());

    let ordinary = Bounds::of_positions(&[at(0.0, 0.0), at(1.0, 1.0)]).expect("non-empty");
    assert!(!ordinary.spans_more_than_half_the_world());
}
