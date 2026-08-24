//! The ingest boundary: what the store will hold, and what it refuses.
//!
//! Two properties are being held here and they are easy to conflate.
//!
//! The first is that **a shape is snapped before it is judged**. Snapping is a
//! transformation and it can create invalidity — two positions distinct in
//! `f64` can land on one grid point — so a shape that is well formed as written
//! can be malformed as stored. Checking validity first would accept every one of
//! those and write them, which is the exact failure the check exists to prevent,
//! arriving through the door the check left open.
//!
//! The second is that **a refusal describes the snapped shape**, not the
//! submitted one. A caller comparing the coordinates in the message against what
//! it sent can then see that quantisation was the cause, rather than reading a
//! complaint about coordinates it recognises and concluding the store is wrong.
//!
//! Nothing below is checked by looking at output and finding it plausible. Every
//! fixture has a known answer decided before the code runs, and the round-trip
//! property is checked against arbitrary input rather than chosen input.

use proptest::prelude::*;
use tessari_geo::accept::{Defect, Refused, Step};
use tessari_geo::{SCALE, accept};
use tessari_types::{Geometry, Polygon, Position, Ring};

/// A position, longitude first — the order the store stores.
fn at(longitude: f64, latitude: f64) -> Position {
    Position::new(longitude, latitude)
}

/// A closed ring from corners given counter-clockwise, closing itself.
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

/// A polygon from a shell and any number of holes.
fn polygon(shell: &[(f64, f64)], holes: &[&[(f64, f64)]]) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior: ring(shell),
        interiors: holes.iter().map(|hole| ring(hole)).collect(),
    })
}

/// The defect a refusal names, for a test that cares which one fired.
fn defect(shape: &Geometry) -> Defect {
    match accept(shape) {
        Err(Refused::Malformed { defect, .. }) => defect,
        other => unreachable!("expected a malformed shape, got {other:?}"),
    }
}

/// A square with a bite taken out of its eastern side — concave on purpose.
fn concave_shell() -> Vec<(f64, f64)> {
    vec![
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 4.0),
        (4.0, 5.0),
        (10.0, 6.0),
        (10.0, 10.0),
        (0.0, 10.0),
    ]
}

// ------------------------------------------------- snapping, and its order

#[test]
fn a_position_finer_than_the_grid_is_snapped_and_then_never_moves_again() {
    // Eleven decimals: two past the grid's nine.
    let submitted = Geometry::Point(at(2.294_481_012_34, 48.858_370_987_65));

    let stored = accept(&submitted).expect("a point in Paris is on the sphere");
    assert_ne!(
        stored, submitted,
        "the grid is finer than the input, so it moved"
    );

    let again = accept(&stored).expect("the snapped point is still on the sphere");
    assert_eq!(again, stored, "a snapped shape snaps to itself");

    let Geometry::Point(position) = stored else {
        unreachable!("a point stays a point")
    };
    let units = position.longitude * SCALE;
    assert!(
        (units - units.round()).abs() < 1e-6,
        "the stored longitude sits on a grid unit, not between two"
    );
}

#[test]
fn the_refusal_describes_the_snapped_shape_and_not_the_submitted_one() {
    // Two longitudes a tenth of a nanodegree apart: distinct as written, one
    // grid point once stored. The ring collapses to a line with no area.
    let submitted = polygon(
        &[
            (0.0, 0.0),
            (1.000_000_000_1, 0.0),
            (1.000_000_000_2, 0.0),
            (0.5, 0.000_000_000_04),
        ],
        &[],
    );

    match accept(&submitted) {
        Err(Refused::Malformed {
            defect: Defect::RepeatedPosition { position },
            at,
        }) => {
            assert_eq!(
                position.longitude, 1.0,
                "the message carries the grid point the two positions became, \
                 not either of the two that were sent"
            );
            assert!(
                at.steps().contains(&Step::Shell),
                "the refusal says which ring: {at}"
            );
        }
        other => unreachable!("expected two positions to have merged, got {other:?}"),
    }
}

#[test]
fn a_shape_that_is_well_formed_as_written_can_be_refused_once_stored() {
    // Every ring below closes, has area, and does not cross itself — as written.
    // The fourth corner is under a nanodegree from the first, so on the grid it
    // becomes the first, and the ring closes one corner early.
    let submitted = polygon(
        &[
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.000_000_000_2, 0.000_000_000_2),
        ],
        &[],
    );

    assert!(
        submitted.is_well_formed(),
        "the precondition of this test: the shape is fine until it is snapped"
    );

    assert!(
        accept(&submitted).is_err(),
        "snapping merged two corners, so the stored ring is not the ring that was sent"
    );
}

// --------------------------------------------------------- off the sphere

#[test]
fn a_longitude_past_the_meridian_is_refused_rather_than_wrapped() {
    let refusal = accept(&Geometry::Point(at(181.0, 0.0))).expect_err("181 is not a longitude");
    let said = refusal.to_string();
    assert!(said.contains("longitude"), "names the axis: {said}");
    assert!(said.contains("181"), "names the value: {said}");
}

#[test]
fn a_latitude_past_the_pole_is_refused_and_names_the_latitude() {
    let refusal = accept(&Geometry::Point(at(0.0, 90.5))).expect_err("90.5 is not a latitude");
    let said = refusal.to_string();
    assert!(said.contains("latitude"), "names the axis: {said}");
}

#[test]
fn a_coordinate_that_is_valid_longitude_first_is_impossible_the_other_way_round() {
    // The reversal test only means something when the reversed pair is
    // detectably wrong. 150 is a longitude and cannot be a latitude, so a layer
    // that swapped the pair would fail here rather than quietly storing a point
    // in the wrong hemisphere.
    accept(&Geometry::Point(at(150.0, 45.0))).expect("longitude 150, latitude 45");
    accept(&Geometry::Point(at(45.0, 150.0)))
        .expect_err("latitude 150 does not exist, so the reversed pair is refused");
}

#[test]
fn a_coordinate_that_is_not_a_number_is_refused_rather_than_chosen_for() {
    assert!(accept(&Geometry::Point(at(f64::NAN, 0.0))).is_err());
    assert!(accept(&Geometry::Point(at(0.0, f64::INFINITY))).is_err());
}

#[test]
fn the_limits_themselves_are_inside_the_sphere() {
    accept(&Geometry::Point(at(180.0, 90.0))).expect("the corner of the world is a place");
    accept(&Geometry::Point(at(-180.0, -90.0))).expect("so is the opposite corner");
}

// -------------------------------------------------------------- structure

#[test]
fn a_line_of_one_position_is_not_a_path() {
    assert_eq!(
        defect(&Geometry::Line(vec![at(0.0, 0.0)])),
        Defect::LineTooShort { had: 1 }
    );
}

#[test]
fn a_ring_that_does_not_close_is_refused() {
    let open = Geometry::Polygon(Polygon {
        exterior: Ring(vec![at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)]),
        interiors: Vec::new(),
    });
    assert_eq!(defect(&open), Defect::RingNotClosed);
}

#[test]
fn a_ring_of_three_positions_cannot_bound_anything() {
    let stub = Geometry::Polygon(Polygon {
        exterior: Ring(vec![at(0.0, 0.0), at(1.0, 0.0), at(0.0, 0.0)]),
        interiors: Vec::new(),
    });
    assert_eq!(defect(&stub), Defect::RingTooShort { had: 3 });
}

#[test]
fn a_ring_with_no_area_is_refused() {
    // Three collinear corners: it closes, it has four positions, and it encloses
    // nothing at all.
    let sliver = polygon(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)], &[]);
    assert_eq!(defect(&sliver), Defect::RingHasNoArea);
}

#[test]
fn a_ring_that_crosses_itself_is_refused() {
    // A bowtie: the store cannot know which half was meant, so it does not guess.
    // Lopsided on purpose — a symmetric bowtie's two lobes cancel, so it would be
    // caught as having no area and this test would prove a different thing.
    let bowtie = polygon(&[(0.0, 0.0), (4.0, 4.0), (4.0, 0.0), (0.0, 6.0)], &[]);
    assert!(
        matches!(defect(&bowtie), Defect::RingSelfIntersects { .. }),
        "a bowtie is the canonical self-intersection"
    );
}

#[test]
fn a_ring_that_touches_itself_at_one_vertex_is_still_refused() {
    // Two lobes meeting at a point. Every edge pair either shares a vertex by
    // adjacency or is disjoint — except the two that meet in the middle, which a
    // straddle test alone cannot see.
    let figure_eight = polygon(
        &[
            (0.0, 0.0),
            (2.0, 2.0),
            (4.0, 0.0),
            (4.0, 4.0),
            (2.0, 2.0),
            (0.0, 4.0),
        ],
        &[],
    );
    assert!(matches!(
        defect(&figure_eight),
        Defect::RingSelfIntersects { .. }
    ));
}

#[test]
fn a_square_with_a_square_hole_is_held() {
    let with_hole = polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[&[(2.0, 2.0), (2.0, 4.0), (4.0, 4.0), (4.0, 2.0)]],
    );
    accept(&with_hole).expect("a hole strictly inside its shell is an ordinary polygon");
}

#[test]
fn a_hole_that_is_not_inside_its_shell_is_refused() {
    let elsewhere = polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[&[(20.0, 20.0), (20.0, 22.0), (22.0, 22.0), (22.0, 20.0)]],
    );
    assert!(matches!(
        defect(&elsewhere),
        Defect::HoleOutsideShell { .. }
    ));
}

#[test]
fn a_hole_whose_corners_are_inside_but_whose_edge_leaves_is_refused() {
    // Every corner of this hole sits inside the concave shell; the edge between
    // the two eastern corners passes straight through the bite. A check that
    // only looked at corners would hold it.
    let leaking = Geometry::Polygon(Polygon {
        exterior: ring(&concave_shell()),
        interiors: vec![ring(&[(6.0, 2.0), (9.0, 2.0), (9.0, 8.0), (6.0, 8.0)])],
    });
    assert!(matches!(defect(&leaking), Defect::HoleOutsideShell { .. }));
}

#[test]
fn two_holes_that_overlap_are_refused() {
    let overlapping = polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[
            &[(2.0, 2.0), (2.0, 6.0), (6.0, 6.0), (6.0, 2.0)],
            &[(4.0, 4.0), (4.0, 8.0), (8.0, 8.0), (8.0, 4.0)],
        ],
    );
    assert!(matches!(defect(&overlapping), Defect::HolesOverlap { .. }));
}

#[test]
fn a_hole_inside_another_hole_is_refused() {
    let nested = polygon(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[
            &[(1.0, 1.0), (1.0, 9.0), (9.0, 9.0), (9.0, 1.0)],
            &[(4.0, 4.0), (4.0, 5.0), (5.0, 5.0), (5.0, 4.0)],
        ],
    );
    assert!(matches!(defect(&nested), Defect::HolesOverlap { .. }));
}

// ------------------------------------------- which way round the world it goes

#[test]
fn a_line_edge_spanning_more_than_half_the_world_is_refused() {
    let across = Geometry::Line(vec![at(179.0, 0.0), at(-179.0, 0.0)]);
    assert!(matches!(
        defect(&across),
        Defect::EdgeSpansHalfTheWorld { .. }
    ));
}

#[test]
fn exactly_half_the_world_is_held() {
    // The boundary, and it belongs on the accepting side: at 180 degrees the two
    // readings have the same length and the same box, so nothing the store can
    // observe distinguishes them and there is no choice being made for anyone.
    accept(&Geometry::Line(vec![at(-90.0, 0.0), at(90.0, 0.0)]))
        .expect("half the world is not more than half the world");

    // And one grid unit past it is not.
    let past = Geometry::Line(vec![at(-90.0, 0.0), at(90.000_000_001, 0.0)]);
    assert!(matches!(
        defect(&past),
        Defect::EdgeSpansHalfTheWorld { .. }
    ));
}

#[test]
fn an_edge_between_two_positions_at_one_pole_is_exempt() {
    // At latitude 90 every longitude is the same place, so both readings of this
    // edge are the same degenerate point. Refusing it would cost the polar cap
    // for no correctness gained.
    accept(&Geometry::Line(vec![at(-180.0, 90.0), at(180.0, 90.0)]))
        .expect("there is no direction to state where there is no direction");
    accept(&Geometry::Line(vec![at(-180.0, -90.0), at(180.0, -90.0)]))
        .expect("and the same at the other pole");
}

#[test]
fn an_edge_from_one_pole_to_the_other_is_not_exempt() {
    // The exemption is about positions at *one* pole. An edge running between
    // them passes through every latitude in between, where longitude means what
    // it usually means, so which way round is a real question again.
    let meridian_to_meridian = Geometry::Line(vec![at(-179.0, 90.0), at(179.0, -90.0)]);
    assert!(matches!(
        defect(&meridian_to_meridian),
        Defect::EdgeSpansHalfTheWorld { .. }
    ));
}

#[test]
fn the_refusal_names_the_offending_edge_at_any_depth() {
    let nested = Geometry::MultiLine(vec![
        vec![at(0.0, 0.0), at(1.0, 1.0)],
        vec![at(0.0, 0.0), at(10.0, 0.0), at(179.0, 0.0), at(-179.0, 0.0)],
    ]);
    match accept(&nested) {
        Err(Refused::Malformed {
            defect: Defect::EdgeSpansHalfTheWorld { from, to },
            at: site,
        }) => {
            assert_eq!((from, to), (at(179.0, 0.0), at(-179.0, 0.0)));
            assert_eq!(
                site.steps(),
                &[Step::Member(1), Step::Position(2)],
                "the third edge of the second line, and the message says so: {site}"
            );
        }
        other => unreachable!("expected the second line's last edge to be refused, got {other:?}"),
    }
}

#[test]
fn a_multi_point_straddling_the_meridian_is_still_held() {
    // Deliberate, and recorded rather than incidental. A set of points states
    // its meaning completely — there is no path between them to be read one way
    // or the other — so there is nothing here for the store to be unsure about.
    // Its box is loose, which costs refinement and never an answer.
    accept(&Geometry::MultiPoint(vec![at(179.0, 0.0), at(-179.0, 0.0)]))
        .expect("points have no edges");
}

// ------------------------------------------------------- where it went wrong

#[test]
fn a_refusal_names_the_member_of_a_collection_that_failed() {
    let mixed = Geometry::Collection(vec![
        Box::new(Geometry::Point(at(0.0, 0.0))),
        Box::new(Geometry::Point(at(1.0, 1.0))),
        Box::new(Geometry::Line(vec![at(2.0, 2.0)])),
    ]);
    match accept(&mixed) {
        Err(Refused::Malformed { at, .. }) => {
            assert_eq!(at.steps().first(), Some(&Step::Member(2)));
        }
        other => unreachable!("expected the third member to be refused, got {other:?}"),
    }
}

#[test]
fn a_refusal_names_which_hole_of_which_polygon() {
    let shell: &[(f64, f64)] = &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
    let good = match polygon(shell, &[&[(1.0, 1.0), (1.0, 2.0), (2.0, 2.0), (2.0, 1.0)]]) {
        Geometry::Polygon(shape) => shape,
        other => unreachable!("built as a polygon, got {other:?}"),
    };
    let bad = match polygon(
        shell,
        &[
            &[(1.0, 1.0), (1.0, 2.0), (2.0, 2.0), (2.0, 1.0)],
            &[(50.0, 50.0), (50.0, 52.0), (52.0, 52.0), (52.0, 50.0)],
        ],
    ) {
        Geometry::Polygon(shape) => shape,
        other => unreachable!("built as a polygon, got {other:?}"),
    };

    match accept(&Geometry::MultiPolygon(vec![good, bad])) {
        Err(Refused::Malformed { at, .. }) => {
            assert_eq!(
                at.steps(),
                &[Step::Member(1), Step::Hole(1)],
                "the second polygon's second hole, and the message says so: {at}"
            );
        }
        other => unreachable!("expected the second polygon to be refused, got {other:?}"),
    }
}

// ---------------------------------------------------------- the round trip

#[test]
fn every_shape_kind_survives_the_boundary() {
    let square: &[(f64, f64)] = &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let far: &[(f64, f64)] = &[(20.0, 20.0), (21.0, 20.0), (21.0, 21.0), (20.0, 21.0)];
    let one = match polygon(square, &[]) {
        Geometry::Polygon(shape) => shape,
        other => unreachable!("built as a polygon, got {other:?}"),
    };
    let two = match polygon(far, &[]) {
        Geometry::Polygon(shape) => shape,
        other => unreachable!("built as a polygon, got {other:?}"),
    };

    for shape in [
        Geometry::Point(at(1.5, 2.5)),
        Geometry::MultiPoint(vec![at(1.0, 2.0), at(3.0, 4.0)]),
        Geometry::Line(vec![at(0.0, 0.0), at(1.0, 1.0)]),
        Geometry::MultiLine(vec![
            vec![at(0.0, 0.0), at(1.0, 1.0)],
            vec![at(2.0, 2.0), at(3.0, 3.0)],
        ]),
        polygon(square, &[]),
        Geometry::MultiPolygon(vec![one, two]),
        Geometry::Collection(vec![
            Box::new(Geometry::Point(at(5.0, 5.0))),
            Box::new(polygon(square, &[])),
        ]),
    ] {
        let stored = accept(&shape).expect("every fixture here is a shape the store holds");
        assert_eq!(
            stored, shape,
            "these coordinates are already on the grid, so nothing should move"
        );
        assert_eq!(
            accept(&stored).expect("a stored shape is acceptable"),
            stored,
            "the boundary is idempotent"
        );
    }
}

proptest! {
    /// Whatever arrives, the second pass moves nothing.
    ///
    /// This is what "lossless at the declared precision" means, and it is the
    /// property a read-modify-write cycle rests on: without it a shape drifts a
    /// little on every cycle, invisibly per cycle and fatally in aggregate.
    #[test]
    fn snapping_is_idempotent_for_any_position(
        longitude in -180.0_f64..=180.0,
        latitude in -90.0_f64..=90.0,
    ) {
        let once = accept(&Geometry::Point(at(longitude, latitude)))
            .expect("the ranges above are the sphere");
        let twice = accept(&once).expect("a snapped point stays on the sphere");
        prop_assert_eq!(twice, once);
    }
}

// ------------------------------------------- the members of a multi-polygon

/// Two squares given as one multi-polygon.
fn two_members(first: &[(f64, f64)], second: &[(f64, f64)]) -> Geometry {
    Geometry::MultiPolygon(vec![
        Polygon {
            exterior: ring(first),
            interiors: Vec::new(),
        },
        Polygon {
            exterior: ring(second),
            interiors: Vec::new(),
        },
    ])
}

#[test]
fn two_members_that_share_area_are_refused_and_named_by_their_positions() {
    // The invariant `covers` rests on. Two overlapping members mean a segment
    // can cross a ring edge and still be inside the shape, which is exactly what
    // the containment rule assumes cannot happen.
    let overlapping = two_members(
        &[(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0)],
        &[(4.0, 4.0), (10.0, 4.0), (10.0, 10.0), (4.0, 10.0)],
    );
    assert_eq!(
        defect(&overlapping),
        Defect::MembersOverlap {
            earlier: 0,
            later: 1
        }
    );
}

#[test]
fn a_member_nested_inside_another_shares_area_and_is_refused() {
    let nested = two_members(
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
        &[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)],
    );
    assert_eq!(
        defect(&nested),
        Defect::MembersOverlap {
            earlier: 0,
            later: 1
        }
    );
}

#[test]
fn two_members_sharing_a_stretch_of_edge_are_refused_but_a_shared_corner_is_not() {
    // A shared edge encloses no area and still breaks the same rule, because a
    // segment crossing it passes from one member's inside to the other's. A
    // shared *corner* does not, and both RFC 7946 and OGC allow it.
    let along_an_edge = two_members(
        &[(0.0, 0.0), (5.0, 0.0), (5.0, 5.0), (0.0, 5.0)],
        &[(5.0, 1.0), (9.0, 1.0), (9.0, 4.0), (5.0, 4.0)],
    );
    assert_eq!(
        defect(&along_an_edge),
        Defect::MembersOverlap {
            earlier: 0,
            later: 1
        }
    );

    let at_a_corner = two_members(
        &[(0.0, 0.0), (5.0, 0.0), (5.0, 5.0), (0.0, 5.0)],
        &[(5.0, 5.0), (9.0, 5.0), (9.0, 9.0), (5.0, 9.0)],
    );
    assert!(accept(&at_a_corner).is_ok(), "a shared corner is legal");
}

#[test]
fn members_that_stay_apart_are_accepted() {
    let apart = two_members(
        &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
        &[(5.0, 5.0), (6.0, 5.0), (6.0, 6.0), (5.0, 6.0)],
    );
    assert!(accept(&apart).is_ok());
}

#[test]
fn a_member_sitting_in_another_members_hole_is_accepted() {
    // The one nesting that is legal: the outer member does not cover the hole,
    // so the two share no area at all. A check that tested shells rather than
    // areas would refuse this, and a doughnut with its own centre is a shape
    // people draw.
    let doughnut_and_its_centre = Geometry::MultiPolygon(vec![
        Polygon {
            exterior: ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]),
            interiors: vec![ring(&[(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0)])],
        },
        Polygon {
            exterior: ring(&[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0)]),
            interiors: Vec::new(),
        },
    ]);
    assert!(accept(&doughnut_and_its_centre).is_ok());
}
