//! A brute-force oracle, and the properties the spatial index will stand on.
//!
//! # Why an oracle rather than more fixtures
//!
//! A fixture list checks the cases somebody thought of. This file checks the
//! cases nobody did, by computing the same answers a **second time with a
//! different algorithm** and comparing. That is the only way geometric code is
//! ever known to be right: every wrong geospatial answer looks exactly like a
//! right one — a point on the correct street in the correct city that happens to
//! be outside the park — so reading a result and finding it plausible proves
//! nothing at all.
//!
//! Two independent implementations can of course be wrong the same way, and no
//! oracle removes that. What reduces it is choosing an algorithm that fails
//! *differently*: [`ring_contains`] counts a signed **winding** number, and the
//! reference here counts **crossing parity**. They agree for every simple ring
//! and disagree for a self-overlapping one, which is a real difference rather
//! than the same idea typed twice — and the ingest boundary guarantees the rings
//! reaching a store are simple.
//!
//! # The property this file exists for
//!
//! `relate::intersects` opens by asking whether the two bounding boxes meet and
//! answers `false` when they do not. So the box is not merely the *future*
//! index's pre-filter — **the predicate already depends on it today**. If
//! [`Bounds::meets`] could answer `false` for two shapes that genuinely share a
//! position, `intersects` would already be quietly wrong, and a spatial index
//! built on the same box would inherit the same missing rows.
//!
//! A missing row is the one failure direction a filter must not have, and it is
//! invisible: nothing raises, the answer is simply short. So the oracle here
//! computes intersection **with no box anywhere in it**, and the soundness of the
//! filter is asserted over every ordered pair of the corpus rather than assumed.

#![allow(clippy::unwrap_used, clippy::panic)]

use tessari_geo::{
    Bounds, Containment, Shape, Snapped, contains, covered_by, covers, disjoint, equals,
    intersects, on_segment, ring_contains, segments_meet, within,
};
use tessari_types::{Geometry, Polygon, Position, Ring};

// ------------------------------------------------------------- the reference

/// Whether `of` is inside `ring`, by **crossing parity** rather than winding.
///
/// Cast a ray east from `of` and count the edges it crosses; an odd count is
/// inside. The half-open rule — an edge counts when exactly one endpoint is
/// strictly above the ray's latitude — is what stops a vertex sitting on the ray
/// from being counted twice or not at all, and it is applied without any
/// tolerance because the coordinates are integers.
///
/// The crossing's longitude is compared **without dividing**: rather than
/// computing where the edge meets the ray, the comparison is rearranged into one
/// `i128` multiplication on each side, so the answer is an exact sign and not a
/// rounded intersection point. Dividing here is the classic way a
/// point-in-polygon test acquires a tolerance it then has to defend.
///
/// `None` when `of` lies on the boundary: parity is undefined there, and the
/// caller checks that case separately rather than being handed a guess.
fn inside_by_parity(ring: &[Snapped], of: Snapped) -> Option<bool> {
    if ring.len() < 4 {
        return Some(false);
    }
    let probe_longitude = i128::from(of.longitude_units());
    let probe_latitude = i128::from(of.latitude_units());

    for edge in ring.windows(2) {
        if on_segment(edge[0], edge[1], of) {
            return None;
        }
    }

    let mut crossings = 0_u32;
    for edge in ring.windows(2) {
        let (from, to) = (edge[0], edge[1]);
        let from_longitude = i128::from(from.longitude_units());
        let from_latitude = i128::from(from.latitude_units());
        let to_longitude = i128::from(to.longitude_units());
        let to_latitude = i128::from(to.latitude_units());

        // Half-open in latitude: exactly one endpoint strictly above the ray.
        let straddles = (from_latitude > probe_latitude) != (to_latitude > probe_latitude);
        if !straddles {
            continue;
        }

        // The edge meets the ray at longitude
        //     from_longitude + (probe_latitude - from_latitude)
        //                      * (to_longitude - from_longitude)
        //                      / (to_latitude - from_latitude)
        // and the question is only whether that is east of the probe. Multiply
        // both sides by the (non-zero) latitude span and compare, flipping the
        // comparison when the span is negative — no division, so no rounding.
        //
        // Saturating rather than bare, matching the kernel's own convention and
        // for the same reason it gives: the workspace refuses arithmetic that
        // could wrap, and neither product can reach the saturation point — a
        // coordinate is bounded by 180 × 10^9, so the largest product here is
        // about 6.5 × 10^22 against `i128`'s 1.7 × 10^38. These are exact
        // products written in a form that says so.
        let latitude_span = to_latitude.saturating_sub(from_latitude);
        let left = probe_latitude
            .saturating_sub(from_latitude)
            .saturating_mul(to_longitude.saturating_sub(from_longitude));
        let right = probe_longitude
            .saturating_sub(from_longitude)
            .saturating_mul(latitude_span);
        let east_of_probe = if latitude_span > 0 {
            left > right
        } else {
            left < right
        };
        if east_of_probe {
            crossings = crossings.saturating_add(1);
        }
    }
    Some(crossings % 2 == 1)
}

/// Whether two shapes share any position, computed with **no bounding box**.
///
/// The same two questions `relate` asks — does a vertex of either lie on the
/// other, and does any pair of boundary segments meet — with the box short-cut
/// removed. That omission is the whole point: this is what the pre-filter is
/// measured against.
fn intersects_without_a_box(one: &Shape, other: &Shape) -> bool {
    let mut here_positions = Vec::new();
    one.each_position(&mut |at| here_positions.push(at));
    let mut there_positions = Vec::new();
    other.each_position(&mut |at| there_positions.push(at));
    if here_positions.is_empty() || there_positions.is_empty() {
        return false;
    }

    if here_positions.iter().any(|at| holds(other, *at))
        || there_positions.iter().any(|at| holds(one, *at))
    {
        return true;
    }

    let mut here_segments = Vec::new();
    one.each_segment(&mut |from, to| here_segments.push((from, to)));
    let mut there_segments = Vec::new();
    other.each_segment(&mut |from, to| there_segments.push((from, to)));

    here_segments.iter().any(|(from, to)| {
        there_segments
            .iter()
            .any(|(other_from, other_to)| segments_meet(*from, *to, *other_from, *other_to))
    })
}

/// Whether `shape` holds `at` — on a boundary, on a line, or inside an area.
///
/// Written against the reference point-in-ring above, so that it fails
/// independently of the kernel's own containment.
fn holds(shape: &Shape, at: Snapped) -> bool {
    let mut found = false;
    shape.each_position(&mut |position| found |= position == at);
    if found {
        return true;
    }
    let mut on_a_segment = false;
    shape.each_segment(&mut |from, to| on_a_segment |= on_segment(from, to, at));
    if on_a_segment {
        return true;
    }
    let mut inside = false;
    shape.each_area(&mut |area| {
        let in_shell = inside_by_parity(&area.shell, at).unwrap_or(true);
        let in_a_hole = area
            .holes
            .iter()
            .any(|hole| inside_by_parity(hole, at) == Some(true));
        inside |= in_shell && !in_a_hole;
    });
    inside
}

// ---------------------------------------------------------------- the corpus

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

/// The shapes the oracle sweeps, each chosen for a way containment goes wrong.
///
/// Named rather than generated, because a generator produces shapes nobody can
/// reason about when one of them fails — and a failing case that cannot be
/// reasoned about gets deleted rather than fixed.
fn corpus() -> Vec<(&'static str, Shape)> {
    let entries: Vec<(&'static str, Geometry)> = vec![
        (
            "a unit square",
            polygon(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)], &[]),
        ),
        (
            "a square with a square hole",
            polygon(
                &[(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0)],
                &[&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)]],
            ),
        ),
        (
            "an L, so a ray leaves and re-enters",
            polygon(
                &[
                    (0.0, 0.0),
                    (4.0, 0.0),
                    (4.0, 1.0),
                    (1.0, 1.0),
                    (1.0, 4.0),
                    (0.0, 4.0),
                ],
                &[],
            ),
        ),
        (
            "a comb, so a ray crosses many times",
            polygon(
                &[
                    (0.0, 0.0),
                    (6.0, 0.0),
                    (6.0, 3.0),
                    (5.0, 3.0),
                    (5.0, 1.0),
                    (4.0, 1.0),
                    (4.0, 3.0),
                    (3.0, 3.0),
                    (3.0, 1.0),
                    (2.0, 1.0),
                    (2.0, 3.0),
                    (1.0, 3.0),
                    (1.0, 1.0),
                    (0.0, 1.0),
                ],
                &[],
            ),
        ),
        (
            "a triangle with a vertex on the sweep line",
            polygon(&[(0.0, 0.0), (4.0, 2.0), (0.0, 4.0)], &[]),
        ),
        (
            "a sliver one grid unit tall",
            polygon(
                &[
                    (0.0, 0.0),
                    (5.0, 0.0),
                    (5.0, 0.000_000_001),
                    (0.0, 0.000_000_001),
                ],
                &[],
            ),
        ),
        (
            "a square touching the first along one edge",
            polygon(&[(2.0, 0.0), (4.0, 0.0), (4.0, 2.0), (2.0, 2.0)], &[]),
        ),
        (
            "a square well away from everything",
            polygon(
                &[(50.0, 50.0), (52.0, 50.0), (52.0, 52.0), (50.0, 52.0)],
                &[],
            ),
        ),
        ("a lone position", Geometry::Point(at(1.0, 1.0))),
        (
            "a line through the first square",
            Geometry::Line(vec![at(-1.0, 1.0), at(3.0, 1.0)]),
        ),
        (
            "a line touching a corner and no more",
            Geometry::Line(vec![at(2.0, 2.0), at(3.0, 3.0)]),
        ),
    ];
    entries
        .into_iter()
        .map(|(name, geometry)| {
            (
                name,
                Shape::of(&geometry).expect("every corpus shape is on the grid"),
            )
        })
        .collect()
}

// ------------------------------------------------- 1. two algorithms, one answer

#[test]
fn winding_and_crossing_parity_agree_on_every_point_of_a_lattice() {
    // The sweep. For each area in the corpus, every point of a lattice over its
    // box, widened so the outside is sampled too. Boundary points are diverted:
    // parity is undefined there, so the assertion becomes "the kernel said
    // Boundary, and the point really does lie on an edge" — checked against the
    // ring's own segments, which is a third route to the same fact.
    const STEPS: i64 = 24;
    let mut inside = 0_u64;
    let mut outside = 0_u64;
    let mut boundary = 0_u64;

    for (name, shape) in corpus() {
        let mut rings: Vec<Vec<Snapped>> = Vec::new();
        shape.each_area(&mut |area| {
            rings.push(area.shell.clone());
            rings.extend(area.holes.iter().cloned());
        });
        for held in &rings {
            let box_of_it = Bounds::of_positions(held).expect("a ring has positions");
            let width = box_of_it.east().saturating_sub(box_of_it.west());
            let height = box_of_it.north().saturating_sub(box_of_it.south());
            // A margin of a fifth of the extent on each side, so the lattice
            // straddles the edge rather than stopping on it.
            let margin_x = (width / 5).max(1);
            let margin_y = (height / 5).max(1);
            let west = box_of_it.west().saturating_sub(margin_x);
            let south = box_of_it.south().saturating_sub(margin_y);
            let span_x = width.saturating_add(margin_x.saturating_mul(2));
            let span_y = height.saturating_add(margin_y.saturating_mul(2));

            for row in 0..=STEPS {
                for column in 0..=STEPS {
                    let longitude = west.saturating_add(span_x.saturating_mul(column) / STEPS);
                    let latitude = south.saturating_add(span_y.saturating_mul(row) / STEPS);
                    let Ok(probe) = Snapped::from_units(longitude, latitude) else {
                        continue;
                    };
                    let kernel = ring_contains(held, probe);
                    match (kernel, inside_by_parity(held, probe)) {
                        (Containment::Boundary, None) => {
                            boundary = boundary.saturating_add(1);
                            assert!(
                                held.windows(2)
                                    .any(|edge| on_segment(edge[0], edge[1], probe)),
                                "{name}: the kernel called ({longitude}, {latitude}) a boundary \
                                 position and it lies on no edge of the ring"
                            );
                        }
                        (Containment::Inside, Some(true)) => {
                            inside = inside.saturating_add(1);
                        }
                        (Containment::Outside, Some(false)) => {
                            outside = outside.saturating_add(1);
                        }
                        (kernel, reference) => panic!(
                            "{name}: at ({longitude}, {latitude}) winding says {kernel:?} \
                             and crossing parity says {reference:?}"
                        ),
                    }
                }
            }
        }
    }

    // A sweep that never got inside, or never got outside, would agree
    // perfectly and prove nothing — the failure mode of every comparison test
    // whose two implementations are only ever handed easy input.
    assert!(
        inside > 0 && outside > 0 && boundary > 0,
        "the lattice did not reach all three answers: inside={inside} outside={outside} \
         boundary={boundary}"
    );
}

// --------------------------------------- 2. the box the index is going to trust

#[test]
fn a_true_intersection_always_has_boxes_that_meet() {
    // The load-bearing property. `relate::intersects` returns `false` as soon as
    // the boxes miss, so a box that could miss while the shapes touch would make
    // the predicate wrong *today* and the index wrong later — silently, as rows
    // that are simply absent from an answer.
    //
    // The oracle used here has no box in it at all, which is what makes this an
    // independent check rather than a restatement.
    let corpus = corpus();
    let mut met = 0_u32;
    for (one_name, one) in &corpus {
        for (other_name, other) in &corpus {
            if !intersects_without_a_box(one, other) {
                continue;
            }
            met = met.saturating_add(1);
            let (Some(one_box), Some(other_box)) = (one.bounds(), other.bounds()) else {
                panic!("{one_name} and {other_name} intersect and one of them has no box");
            };
            assert!(
                one_box.meets(other_box),
                "{one_name} and {other_name} share a position and their boxes do not meet — \
                 the pre-filter would drop this pair"
            );
        }
    }
    assert!(
        met > 0,
        "no pair intersected, so the property was never exercised"
    );
}

#[test]
fn the_predicate_and_the_box_free_oracle_agree_on_every_pair() {
    // The other direction: the box short-cut must not *add* intersections
    // either, and the two algorithms must agree pair by pair rather than in
    // aggregate. Reported per pair, because a count that matches can still be
    // two errors cancelling.
    for (one_name, one) in &corpus() {
        for (other_name, other) in &corpus() {
            assert_eq!(
                intersects(one, other),
                intersects_without_a_box(one, other),
                "{one_name} vs {other_name}: the predicate and the box-free oracle disagree"
            );
        }
    }
}

// ------------------------------------------------- 3. the predicates as an algebra

#[test]
fn the_seven_predicates_satisfy_the_identities_that_define_them() {
    // Each predicate is computed on its own, so these are not restatements of
    // one implementation — they are seven answers that have to be mutually
    // consistent. An error in any one of them shows up as a broken identity
    // even when the individual answer looks reasonable.
    let corpus = corpus();
    for (one_name, one) in &corpus {
        for (other_name, other) in &corpus {
            let pair = format!("{one_name} vs {other_name}");

            assert_eq!(
                disjoint(one, other),
                !intersects(one, other),
                "{pair}: disjoint is not the negation of intersects"
            );
            assert_eq!(
                intersects(one, other),
                intersects(other, one),
                "{pair}: intersects is not symmetric"
            );
            assert_eq!(
                covered_by(one, other),
                covers(other, one),
                "{pair}: covered_by is not covers with the arguments swapped"
            );
            assert_eq!(
                within(one, other),
                contains(other, one),
                "{pair}: within is not contains with the arguments swapped"
            );
            assert_eq!(
                equals(one, other),
                covers(one, other) && covers(other, one),
                "{pair}: equals is not mutual covering"
            );
            assert!(
                !contains(one, other) || covers(one, other),
                "{pair}: contains without covers — the strict half escaped the permissive one"
            );
            assert!(
                !covers(one, other) || intersects(one, other),
                "{pair}: covers without intersects"
            );
        }
    }
}

#[test]
fn covers_and_contains_actually_differ_somewhere_in_the_corpus() {
    // The identities above are consistency checks, and consistency cannot see
    // two predicates **merging**. Collapsing `contains` into `covers` — dropping
    // the boundary exclusion — leaves every one of them satisfied: `contains`
    // still implies `covers`, and `within` is still `contains` with the
    // arguments swapped. Verified by doing it, not assumed: the identities pass
    // and five fixture tests in `relate.rs` fail.
    //
    // So this is the row that notices. Somewhere in the corpus a shape must be
    // covered by another and not contained in it, or the boundary — the whole
    // reason both predicates exist — has stopped being computed.
    let corpus = corpus();
    let differing: Vec<String> = corpus
        .iter()
        .flat_map(|(one_name, one)| {
            corpus
                .iter()
                .filter(move |(_, other)| covers(one, other) && !contains(one, other))
                .map(move |(other_name, _)| {
                    format!("{one_name} covers but does not contain {other_name}")
                })
        })
        .collect();
    assert!(
        !differing.is_empty(),
        "no pair in the corpus separates covers from contains, so the boundary rule \
         could have collapsed without any identity noticing"
    );
}

#[test]
fn every_shape_covers_and_equals_itself() {
    // Reflexivity, which sounds trivial and is the first thing a boundary rule
    // breaks: a shape whose own edge is not counted as covered stops covering
    // itself, and nothing else in the suite would say so.
    for (name, shape) in &corpus() {
        assert!(covers(shape, shape), "{name} does not cover itself");
        assert!(equals(shape, shape), "{name} does not equal itself");
        assert!(intersects(shape, shape), "{name} does not intersect itself");
        assert!(!disjoint(shape, shape), "{name} is disjoint from itself");
    }
}

// --------------------------------------- 4. the scan the index will be measured on

/// Every shape in the corpus that shares a position with `query`, by scanning.
///
/// The answer a spatial index has to reproduce exactly. It is a scan on purpose:
/// the index is faster and must not be different, and the only way to know it is
/// not different is to have the slow answer to compare against.
fn matching<'a>(query: &Shape, corpus: &'a [(&'static str, Shape)]) -> Vec<&'a str> {
    corpus
        .iter()
        .filter(|(_, shape)| intersects_without_a_box(query, shape))
        .map(|(name, _)| *name)
        .collect()
}

#[test]
fn the_brute_force_scan_answers_and_its_answer_is_ordered_by_the_corpus() {
    let corpus = corpus();
    let query = Shape::of(&polygon(
        &[(1.0, 1.0), (3.0, 1.0), (3.0, 3.0), (1.0, 3.0)],
        &[],
    ))
    .unwrap();

    let found = matching(&query, &corpus);

    // Named rather than counted: a count is the summary verdict this project
    // refuses everywhere else, and it would pass while the scan returned the
    // wrong shapes in the right number.
    assert!(found.contains(&"a unit square"), "{found:?}");
    assert!(found.contains(&"a square with a square hole"), "{found:?}");
    assert!(
        found.contains(&"a square touching the first along one edge"),
        "{found:?}"
    );
    assert!(found.contains(&"a lone position"), "{found:?}");
    assert!(
        found.contains(&"a line through the first square"),
        "{found:?}"
    );
    assert!(
        found.contains(&"a line touching a corner and no more"),
        "{found:?}"
    );
    assert!(
        !found.contains(&"a square well away from everything"),
        "the scan reached a shape forty degrees away: {found:?}"
    );

    // And the property that makes it usable as an oracle: the scan and the
    // engine's own predicate answer the same set.
    let engine: Vec<&str> = corpus
        .iter()
        .filter(|(_, shape)| intersects(&query, shape))
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        found, engine,
        "the scan and the predicate answer differently"
    );
}
