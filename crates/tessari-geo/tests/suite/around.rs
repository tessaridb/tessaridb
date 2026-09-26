//! The widened box of a radius read, checked against the distance itself.
//!
//! The oracle measures: it draws positions around a query box, keeps the ones
//! Vincenty puts within `r` of a position in the box, and asserts every one of
//! them is inside the widened box. It shares nothing with the widening but the
//! datum. The draws are concentrated near where the box's edges should fall, and
//! the corpus covers the equator, mid-latitudes, a few kilometres from a pole
//! and the ±180 meridian, at radii from a metre to two thousand kilometres.

#![allow(clippy::unwrap_used)]
#![expect(
    clippy::cast_precision_loss,
    reason = "generator draws are integers far below 2^53"
)]

use tessari_geo::{Bounds, Snapped, distance, within_reach};
use tessari_types::Position;

fn at(longitude: f64, latitude: f64) -> Snapped {
    Snapped::of(Position::new(longitude, latitude)).unwrap()
}

struct Draws(u64);

impl Draws {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn within(&mut self, low: f64, high: f64) -> f64 {
        low + self.next() * (high - low)
    }
}

#[test]
fn every_position_within_the_distance_is_inside_the_widened_box() {
    let mut draws = Draws(0x7261_6469_7573);
    // (a corner of the query box, its size in degrees, the radius in metres)
    let cases: [((f64, f64), f64, f64); 9] = [
        ((10.0, 0.0), 0.0, 1.0),
        ((2.35, 48.85), 0.0, 5_000.0),
        ((2.2, 48.8), 0.3, 20_000.0),
        ((-70.0, -33.0), 1.0, 300_000.0),
        ((25.0, 70.0), 0.5, 150_000.0),
        ((40.0, 89.9), 0.0, 30_000.0),
        ((179.95, 10.0), 0.0, 20_000.0),
        ((-179.99, -45.0), 0.01, 5_000.0),
        ((100.0, 30.0), 2.0, 2_000_000.0),
    ];
    let mut inside = 0_u32;
    for ((west, south), size, radius) in cases {
        let corner = at(west, south);
        let far = at((west + size).min(180.0), (south + size).min(90.0));
        let query = Bounds::of_position(corner).widened_to(far);
        let widened = within_reach(query, radius).unwrap();
        // How far a draw strays, in degrees: somewhat beyond what the radius can
        // reach anywhere, so draws land on both sides of every edge.
        let spread = (radius / 111_000.0 * 3.0).max(1e-6) + size;
        for _ in 0..4_000 {
            let from = at(
                draws.within(west, (west + size).min(180.0)),
                draws.within(south, (south + size).min(90.0)),
            );
            let mut longitude = from.to_position().longitude + draws.within(-spread, spread) * 4.0;
            longitude = ((longitude + 180.0).rem_euclid(360.0)) - 180.0;
            let latitude =
                (from.to_position().latitude + draws.within(-spread, spread)).clamp(-90.0, 90.0);
            let there = at(longitude, latitude);
            let Some(metres) = distance(from, there) else {
                continue;
            };
            if metres <= radius {
                inside += 1;
                assert!(
                    widened.holds_position(there),
                    "({longitude}, {latitude}) is {metres} m from ({:?}) and outside {widened:?} for r = {radius}",
                    from.to_position()
                );
            }
        }
    }
    assert!(
        inside > 3_000,
        "only {inside} draws landed within the radius"
    );
}

#[test]
fn a_negative_or_unbounded_radius_is_not_a_box() {
    let query = Bounds::of_position(at(0.0, 0.0));
    assert_eq!(within_reach(query, -1.0), None);
    assert_eq!(within_reach(query, f64::INFINITY), None);
    assert_eq!(within_reach(query, f64::NAN), None);
}

#[test]
fn a_widening_that_reaches_a_pole_or_the_date_line_spans_every_longitude() {
    let polar = within_reach(Bounds::of_position(at(40.0, 89.9)), 30_000.0).unwrap();
    let dateline = within_reach(Bounds::of_position(at(179.99, 0.0)), 5_000.0).unwrap();
    for (widened, latitude) in [(polar, 89.9), (dateline, 0.0)] {
        assert!(widened.holds_position(at(-180.0, latitude)));
        assert!(widened.holds_position(at(180.0, latitude)));
    }
    // And one that does neither stays narrow.
    let paris = within_reach(Bounds::of_position(at(2.35, 48.85)), 5_000.0).unwrap();
    assert!(!paris.holds_position(at(3.0, 48.85)));
}

#[test]
fn a_tall_box_is_widened_by_its_most_poleward_parallel() {
    // From 60°N to 80°N, a turn in longitude costs far less ground at the top of
    // the box than at the bottom. A position at 80°N, seven and a half degrees east
    // of the box, is well within 200 km of the box's north-east corner — so the
    // widening must be taken where the box is nearest the axis, not where it is
    // furthest from it.
    let query = Bounds::of_position(at(10.0, 60.0)).widened_to(at(12.0, 80.0));
    let there = at(19.5, 80.0);
    let metres = distance(at(12.0, 80.0), there).unwrap();
    assert!(metres < 200_000.0, "{metres}");
    assert!(
        within_reach(query, 200_000.0)
            .unwrap()
            .holds_position(there)
    );
}
