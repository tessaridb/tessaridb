//! The golden corpus: the shapes that separate a correct engine from a lucky one.
//!
//! Every entry here is one of the cases ADR-0026 names, and each one's answer was
//! decided before the code that produces it was written. That order is the whole
//! point. **Every wrong geospatial answer looks exactly like a right one** — there
//! is no exception, no NaN and no error, just a plausible shape in a plausible
//! place — so an engine is never validated by running it and reading the output.
//!
//! One entry records an answer that is *not* the one a person wants, and it is
//! here precisely for that reason. A shape reaching the pole gets a planar
//! answer, and a planar answer over degrees is wrong there in a way nothing
//! reports. Writing the current answer down is what turns a silent limit into a
//! visible one, and what makes the wave that fixes it fail this file rather than
//! quietly change behaviour nobody was watching.
//!
//! The antimeridian entry above it used to be the second of those. It is now a
//! refusal, which is the mechanism working as intended: an entry recording a
//! limit is what makes the fixing wave land here first.
//!
//! The corpus lives as tests rather than as a data file because it has exactly one
//! consumer. When the index arrives and wants the same shapes, it moves.

use tessari_geo::accept::{Defect, Refused, Step};
use tessari_geo::predicate::ring_contains;
use tessari_geo::{Containment, Snapped, accept};
use tessari_types::{Geometry, Polygon, Position, Ring};

fn at(longitude: f64, latitude: f64) -> Position {
    Position::new(longitude, latitude)
}

fn ring(corners: &[(f64, f64)]) -> Ring {
    let mut positions: Vec<Position> = corners
        .iter()
        .map(|&(longitude, latitude)| at(longitude, latitude))
        .collect();
    if let Some(first) = positions.first().copied() {
        positions.push(first);
    }
    Ring(positions)
}

fn polygon(shell: &[(f64, f64)], holes: &[&[(f64, f64)]]) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior: ring(shell),
        interiors: holes.iter().map(|hole| ring(hole)).collect(),
    })
}

fn on_grid(longitude: f64, latitude: f64) -> Snapped {
    Snapped::of(at(longitude, latitude)).expect("a corpus fixture is on the sphere")
}

// ------------------------------------------------------ 1. the antimeridian

#[test]
fn an_antimeridian_crossing_polygon_is_refused_because_which_way_round_is_not_stated() {
    // A person writing this means a narrow strip across the date line. Read as a
    // plane, it is a band spanning 358 degrees the other way — the complement of
    // what was meant, and both readings are perfectly valid shapes.
    //
    // So the coordinates do not say which shape they are. The store keeps
    // neither, for the same reason it keeps neither interior of a bowtie.
    let strip = polygon(
        &[(179.0, 0.0), (-179.0, 0.0), (-179.0, 1.0), (179.0, 1.0)],
        &[],
    );

    match accept(&strip) {
        Err(Refused::Malformed {
            defect: Defect::EdgeSpansHalfTheWorld { from, to },
            at: site,
        }) => {
            assert_eq!((from, to), (at(179.0, 0.0), at(-179.0, 0.0)));
            assert_eq!(
                site.steps(),
                [Step::Shell, Step::Position(0)],
                "the refusal points at the offending edge, not at the shape"
            );
        }
        other => unreachable!("an edge from 179 to -179 has two readings, got {other:?}"),
    }
}

#[test]
fn both_ways_of_saying_what_was_meant_are_held() {
    // The refusal above costs the caller nothing they cannot express, and this
    // is the other half of the recorded answer: each intended shape has a
    // writing whose reading is unique.

    // The short way — two polygons meeting at the meridian, which is what
    // RFC 7946 asks producers to do anyway.
    let east = polygon(
        &[(179.0, 0.0), (180.0, 0.0), (180.0, 1.0), (179.0, 1.0)],
        &[],
    );
    let west = polygon(
        &[(-180.0, 0.0), (-179.0, 0.0), (-179.0, 1.0), (-180.0, 1.0)],
        &[],
    );
    accept(&east).expect("a one-degree strip east of the meridian");
    accept(&west).expect("a one-degree strip west of it");

    // The long way — one position between the ends, after which every edge is
    // under half the world and there is nothing left to choose between.
    let band = polygon(
        &[
            (179.0, 0.0),
            (0.0, 0.0),
            (-179.0, 0.0),
            (-179.0, 1.0),
            (0.0, 1.0),
            (179.0, 1.0),
        ],
        &[],
    );
    accept(&band).expect("a band round the world, said so that it can only mean that");
}

// ------------------------------------------------------------ 2. the pole

#[test]
fn a_polar_cap_is_held_and_means_a_rectangle_rather_than_a_cap() {
    // The northernmost degree of the world, written the only way lat/lon allows:
    // a rectangle from -180 to 180 between 89 and 90. It is a valid planar ring
    // and the store holds it. It is not a cap, because a cap has no lat/lon
    // rectangle, and nothing here pretends otherwise.
    let cap = polygon(
        &[
            (-180.0, 89.0),
            (-90.0, 89.0),
            (0.0, 89.0),
            (90.0, 89.0),
            (180.0, 89.0),
            (180.0, 90.0),
            (-180.0, 90.0),
        ],
        &[],
    );
    accept(&cap).expect("a planar ring at the top of the world is still a planar ring");

    // The two seams that a planar reading cannot join: the pole is one point and
    // this shape gives it a whole edge, and the meridian at ±180 is one line the
    // shape treats as two sides.
    assert_ne!(
        on_grid(-180.0, 90.0),
        on_grid(180.0, 90.0),
        "the north pole is one place, and on this grid it has many names — the \
         limit this entry exists to record"
    );
}

// -------------------------------------------------- 3. a ring crossing itself

#[test]
fn a_self_intersecting_ring_is_refused_rather_than_guessed_at() {
    let bowtie = polygon(&[(0.0, 0.0), (4.0, 4.0), (4.0, 0.0), (0.0, 6.0)], &[]);
    match accept(&bowtie) {
        Err(Refused::Malformed {
            defect: Defect::RingSelfIntersects { .. },
            ..
        }) => {}
        other => unreachable!("a bowtie has two possible interiors, got {other:?}"),
    }
}

// ------------------------------------------------- 4. a hole touching its shell

#[test]
fn a_hole_touching_its_shell_at_one_point_is_held() {
    // Legal: the hole rests against the boundary without leaving. The store must
    // not confuse resting against with reaching through, which is why touching
    // and crossing are two different tests rather than one.
    let touching = polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[&[(0.0, 5.0), (3.0, 3.0), (6.0, 5.0), (3.0, 7.0)]],
    );
    accept(&touching).expect("a hole may touch its shell");
}

#[test]
fn a_hole_reaching_through_its_shell_is_refused() {
    let through = polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[&[(-2.0, 4.0), (3.0, 3.0), (6.0, 5.0), (3.0, 7.0)]],
    );
    match accept(&through) {
        Err(Refused::Malformed {
            defect: Defect::HoleOutsideShell { .. },
            ..
        }) => {}
        other => unreachable!("the first corner is outside the shell, got {other:?}"),
    }
}

// -------------------------------------------------------- 5. the zero-area sliver

#[test]
fn a_zero_area_sliver_is_refused() {
    let flat = polygon(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)], &[]);
    assert!(matches!(
        accept(&flat),
        Err(Refused::Malformed {
            defect: Defect::RingHasNoArea,
            ..
        })
    ));
}

#[test]
fn a_sliver_the_grid_creates_is_refused_the_same_way() {
    // Written, this ring has area. Stored, its second and third corners are one
    // grid point and it has none. The refusal is the point of the entry: the
    // shape the store judges is the shape the store would keep.
    let vanishing = polygon(
        &[
            (0.0, 0.0),
            (5.0, 0.000_000_000_2),
            (5.0, 0.000_000_000_3),
            (10.0, 0.0),
        ],
        &[],
    );
    assert!(
        matches!(accept(&vanishing), Err(Refused::Malformed { .. })),
        "two corners a third of a nanodegree apart are one corner here"
    );
}

// ------------------------------------------------- 6. a point on the boundary

#[test]
fn a_point_exactly_on_a_boundary_is_neither_inside_nor_outside() {
    // The single most common geo bug report, and both answers are correct: this
    // position is excluded by `contains` and included by `covers`. A layer that
    // folded the boundary into one of the other two answers could not express
    // both, so the kernel returns three answers rather than two.
    let square = [
        on_grid(0.0, 0.0),
        on_grid(10.0, 0.0),
        on_grid(10.0, 10.0),
        on_grid(0.0, 10.0),
        on_grid(0.0, 0.0),
    ];

    assert_eq!(
        ring_contains(&square, on_grid(5.0, 0.0)),
        Containment::Boundary,
        "on an edge"
    );
    assert_eq!(
        ring_contains(&square, on_grid(0.0, 0.0)),
        Containment::Boundary,
        "on a corner"
    );
    assert_eq!(
        ring_contains(&square, on_grid(5.0, 5.0)),
        Containment::Inside
    );
    assert_eq!(
        ring_contains(&square, on_grid(5.0, -0.000_000_001)),
        Containment::Outside,
        "one grid unit below the edge, and the answer changes — there is no \
         tolerance anywhere for it to fall inside"
    );
}

// ------------------------------------------------ 7. the precision limit

#[test]
fn the_corners_of_the_world_are_held_and_a_step_past_them_is_not() {
    accept(&Geometry::Point(at(180.0, 90.0))).expect("the corner of the world");
    accept(&Geometry::Point(at(-180.0, -90.0))).expect("the opposite corner");

    assert!(
        accept(&Geometry::Point(at(180.000_000_001, 0.0))).is_err(),
        "one grid unit past the meridian is off the sphere, not wrapped to -180"
    );
    assert!(accept(&Geometry::Point(at(0.0, 90.000_000_001))).is_err());
}

#[test]
fn two_positions_one_grid_unit_apart_stay_two_positions() {
    // The resolution claim, checked rather than asserted: a nanodegree is about
    // a tenth of a millimetre at the equator, and the store keeps that apart.
    assert_ne!(on_grid(0.0, 0.0), on_grid(0.000_000_001, 0.0));
    assert_eq!(
        on_grid(0.000_000_000_4, 0.0),
        on_grid(0.0, 0.0),
        "and anything finer than half a unit is the same place"
    );
}
