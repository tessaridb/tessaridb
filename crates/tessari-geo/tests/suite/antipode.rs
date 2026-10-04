//! The zone where a distance may not converge holds every pair that does not
//! (G058 C1): a walk that asks it first can never rank a `NONE` distance wrong.

#![allow(clippy::unwrap_used)]

use tessari_geo::{Bounds, Snapped, antipodal_zone, distance};
use tessari_types::Position;

fn at(longitude: f64, latitude: f64) -> Snapped {
    Snapped::of(Position::new(longitude, latitude)).unwrap()
}

#[test]
fn every_position_a_distance_fails_from_is_inside_the_zone() {
    let mut failed = 0_u32;
    for (longitude, latitude) in [(0.0, 0.0), (37.3, 5.0), (120.0, -30.0), (-179.5, 45.0)] {
        let target = at(longitude, latitude);
        let zone = antipodal_zone(target);
        let antipode_longitude = if longitude > 0.0 {
            longitude - 180.0
        } else {
            longitude + 180.0
        };
        for i in -50_i32..=50 {
            for j in -50_i32..=50 {
                let mut there = antipode_longitude + f64::from(i) * 0.05;
                if there > 180.0 {
                    there -= 360.0;
                }
                if there < -180.0 {
                    there += 360.0;
                }
                let place = at(there, (-latitude + f64::from(j) * 0.05).clamp(-90.0, 90.0));
                if distance(target, place).is_none() {
                    failed = failed.saturating_add(1);
                    let point = Bounds::of_position(place);
                    assert!(
                        zone.iter().any(|held| held.meets(point)),
                        "({there}, …) failed outside the zone from ({longitude}, {latitude})"
                    );
                }
            }
        }
    }
    // The sweep reached the failing pairs at all — otherwise it proves nothing.
    assert!(failed > 0, "no near-antipodal pair failed to converge");
}
