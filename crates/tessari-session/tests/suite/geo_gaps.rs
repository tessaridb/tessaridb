//! The four geo gaps a places application meets (G039), on memory and on disk.
//!
//! - a nearest-first read over areas, measured to each area's nearest point;
//! - a radius read served by the spatial index and answering the scan's records;
//! - text and place in one statement;
//! - aggregation by the index's own cells.
//!
//! Every served read is compared with the same read over a twin table that has
//! no index, holding the same records — the scan is the oracle, and a store that
//! never touched the index would pass that comparison, so the plan is asserted
//! beside it.

use std::collections::BTreeMap;

use tessari_session::{Outcome, Session};
use tessari_types::{Geometry, Number, RecordId, Value};

use super::key_value::{on_each_backend, opened, run};

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    session
        .run(read)
        .unwrap_or_else(|error| panic!("{read}: {error}"))
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

fn sorted(mut found: Vec<RecordId>) -> Vec<RecordId> {
    found.sort();
    found
}

fn explained(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let Outcome::Value(Value::Object(fields)) = run(session, &format!("EXPLAIN {read}")) else {
        panic!("a plan is an object");
    };
    match fields.get(field) {
        Some(Value::String(text)) => text.clone(),
        other => format!("{other:?}"),
    }
}

fn point(longitude: f64, latitude: f64) -> String {
    format!("geometry {{ type: 'Point', coordinates: [{longitude}, {latitude}] }}")
}

/// A skewed world: a dense cluster in a city, a sparse spread across the globe,
/// a handful near the north pole and on both sides of ±180, and a few paths and
/// areas — written into `served` (with a spatial index) and `bare` (without).
fn world(session: &mut Session<'_>) {
    let mut script = String::from(
        "DEFINE TABLE served SCHEMALESS; DEFINE TABLE bare SCHEMALESS; \
         DEFINE INDEX by_at ON served FIELDS at SPATIAL;",
    );
    let mut seed = 0x6765_6f67_6170_u64;
    let mut draw = || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from(u32::try_from(seed >> 40).unwrap()) / f64::from(1_u32 << 24)
    };
    let mut shapes = Vec::new();
    for _ in 0..120 {
        shapes.push(point(2.2 + draw() * 0.3, 48.75 + draw() * 0.2));
    }
    for _ in 0..60 {
        shapes.push(point(draw() * 360.0 - 180.0, draw() * 170.0 - 85.0));
    }
    for _ in 0..15 {
        shapes.push(point(draw() * 360.0 - 180.0, 89.9 + draw() * 0.09));
    }
    for _ in 0..15 {
        let side = if draw() < 0.5 { 179.9 } else { -180.0 };
        shapes.push(point(side + draw() * 0.1, draw() * 2.0 - 1.0));
    }
    shapes.push(
        "geometry { type: 'LineString', coordinates: [[2.30, 48.80], [2.40, 48.82]] }".to_owned(),
    );
    shapes.push(
        "geometry { type: 'Polygon', coordinates: [[[2.31, 48.86], [2.33, 48.86], \
         [2.33, 48.87], [2.31, 48.87], [2.31, 48.86]]] }"
            .to_owned(),
    );
    for (n, shape) in shapes.iter().enumerate() {
        let kind = if shape.contains("'Point'") {
            "point"
        } else {
            "shape"
        };
        for table in ["served", "bare"] {
            script.push_str(&format!(
                " CREATE {table}:{n} = {{ at: {shape}, kind: '{kind}' }};"
            ));
        }
    }
    run(session, &script);
}

#[test]
fn a_radius_read_is_served_by_the_index_and_answers_the_scans_records() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        world(&mut session);
        let places = [
            (2.35, 48.85, 1_500.0),
            (2.35, 48.85, 12_000.0),
            (2.36, 48.81, 300.0),
            (0.0, 89.95, 20_000.0),
            (179.99, 0.0, 30_000.0),
            (-179.99, 0.5, 60_000.0),
            (18.9, 69.6, 800_000.0),
            (-60.0, -20.0, 3_000_000.0),
        ];
        let mut answered = 0_usize;
        for (longitude, latitude, radius) in places {
            let here = point(longitude, latitude);
            for condition in [
                format!("geo::distance(at, {here}) < {radius}"),
                format!("geo::distance({here}, at) <= {radius}"),
                format!("{radius} > geo::distance(at, {here})"),
                format!("{radius} >= geo::distance({here}, at)"),
            ] {
                let served = format!("SELECT * FROM served WHERE {condition};");
                let scanned = format!("SELECT * FROM bare WHERE {condition};");
                let found = sorted(ids(&mut session, &served));
                assert_eq!(
                    found,
                    sorted(ids(&mut session, &scanned)),
                    "{}: {condition}",
                    backend.name
                );
                assert_eq!(
                    explained(&mut session, &served, "shape"),
                    "region",
                    "{}: {condition} was not served by the index",
                    backend.name
                );
                answered = answered.saturating_add(found.len());
            }
        }
        assert!(
            answered > 100,
            "{}: only {answered} rows in all",
            backend.name
        );
    });
}

#[test]
fn a_distance_from_below_or_to_a_record_field_is_not_a_radius() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        world(&mut session);
        // `> r` is the outside of a disc and has no box; a radius read from the
        // record names no box either. Both are exact scans, and still answer.
        for condition in [
            format!("geo::distance(at, {}) > 5000", point(2.35, 48.85)),
            format!("geo::distance(at, {}) < at", point(2.35, 48.85)),
        ] {
            let read = format!("SELECT * FROM served WHERE {condition};");
            assert_eq!(
                explained(&mut session, &read, "access"),
                "scan",
                "{}: {condition}",
                backend.name
            );
        }
    });
}

#[test]
fn the_nearest_few_over_areas_answers_the_scans_order() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        world(&mut session);
        for (longitude, latitude) in [(2.32, 48.865), (2.35, 48.81), (0.0, 0.0)] {
            let here = point(longitude, latitude);
            let served =
                format!("SELECT * FROM served ORDER BY geo::distance(at, {here}) LIMIT 5;");
            let scanned = format!("SELECT * FROM bare ORDER BY geo::distance(at, {here}) LIMIT 5;");
            assert_eq!(
                ids(&mut session, &served),
                ids(&mut session, &scanned),
                "{}: from ({longitude}, {latitude})",
                backend.name
            );
        }
        // The area and the path are among the nearest to a place inside the area.
        let inside = ids(
            &mut session,
            &format!(
                "SELECT * FROM served ORDER BY geo::distance(at, {}) LIMIT 1;",
                point(2.32, 48.865)
            ),
        );
        assert_eq!(inside, vec![RecordId::from(211)], "{}", backend.name);
    });
}

#[test]
fn a_name_and_a_place_are_asked_in_one_statement() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            &format!(
                "DEFINE ANALYZER plain FILTERS lowercase;\n\
                 DEFINE TABLE cafes SCHEMALESS;\n\
                 DEFINE FIELD name ON cafes TYPE string ANALYZER plain;\n\
                 DEFINE INDEX by_name ON cafes FIELDS name SEARCH;\n\
                 DEFINE INDEX by_at ON cafes FIELDS at SPATIAL;\n\
                 CREATE cafes:1 = {{ name: 'Blue Door Cafe', at: {} }};\n\
                 CREATE cafes:2 = {{ name: 'Cafe Cafe Blue', at: {} }};\n\
                 CREATE cafes:3 = {{ name: 'Blue Bakery', at: {} }};\n\
                 CREATE cafes:4 = {{ name: 'Red Cafe', at: {} }};\n\
                 CREATE cafes:5 = {{ name: 'Blue Cafe Far Away', at: {} }};",
                point(2.351, 48.851),
                point(2.36, 48.86),
                point(2.3505, 48.8505),
                point(2.3502, 48.8502),
                point(13.4, 52.5),
            ),
        );
        let here = point(2.35, 48.85);
        // Ranked together: the text score and the distance fused by rank.
        let fused = ids(
            &mut session,
            &format!(
                "SELECT * FROM cafes WHERE name MATCHES 'blue cafe' \
                 ORDER BY FUSE (search::score(name, 'blue cafe') DESC, \
                 geo::distance(at, {here})) LIMIT 3;"
            ),
        );
        assert_eq!(
            fused,
            [1, 2, 5].map(RecordId::from).to_vec(),
            "{}",
            backend.name
        );
        // Or filtered by name within a radius, nearest first.
        let near = ids(
            &mut session,
            &format!(
                "SELECT * FROM cafes WHERE name MATCHES 'cafe' \
                 AND geo::distance(at, {here}) < 2000 \
                 ORDER BY geo::distance(at, {here}) LIMIT 10;"
            ),
        );
        assert_eq!(
            near,
            [4, 1, 2].map(RecordId::from).to_vec(),
            "{}",
            backend.name
        );
    });
}

#[test]
fn grouping_by_cell_counts_what_each_cell_holds() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        world(&mut session);
        for level in [0_u32, 3, 9, 14] {
            let Outcome::Records { records, .. } = run(
                &mut session,
                &format!(
                    "SELECT geo::cell(at, {level}) AS cell, count(*) AS n FROM bare \
                     WHERE kind = 'point' GROUP BY geo::cell(at, {level});"
                ),
            ) else {
                panic!("a grouped read answers with records");
            };
            let mut counted = BTreeMap::new();
            for (_, value) in &records {
                let Value::Object(fields) = value else {
                    panic!("a group is an object")
                };
                let (Some(Value::Geometry(cell)), Some(Value::Number(Number::Integer(n)))) =
                    (fields.get("cell"), fields.get("n"))
                else {
                    panic!("expected a cell and a count, got {fields:?}")
                };
                counted.insert(format!("{cell:?}"), *n);
            }
            assert_eq!(
                counted,
                expected_cells(&mut session, level),
                "{}",
                backend.name
            );
        }
    });
}

/// The same grouping, computed test-side from each position's cell.
fn expected_cells(session: &mut Session<'_>, level: u32) -> BTreeMap<String, i64> {
    let Outcome::Records { records, .. } = run(session, "SELECT at FROM bare;") else {
        panic!("a read answers with records");
    };
    let mut counted = BTreeMap::new();
    for (_, value) in &records {
        let Value::Object(fields) = value else {
            panic!("a record is an object")
        };
        let Some(Value::Geometry(Geometry::Point(position))) = fields.get("at") else {
            continue;
        };
        let snapped = tessari_geo::Snapped::of(*position).unwrap();
        let extent = tessari_geo::Cell::containing(snapped)
            .ancestor(level)
            .unwrap()
            .extent()
            .unwrap();
        let low = tessari_geo::Snapped::from_units(extent.west(), extent.south())
            .unwrap()
            .to_position();
        let high = tessari_geo::Snapped::from_units(extent.east(), extent.north())
            .unwrap()
            .to_position();
        let cell = Geometry::Polygon(tessari_types::Polygon {
            exterior: tessari_types::Ring(vec![
                low,
                tessari_types::Position::new(high.longitude, low.latitude),
                high,
                tessari_types::Position::new(low.longitude, high.latitude),
                low,
            ]),
            interiors: Vec::new(),
        });
        let held = counted.entry(format!("{cell:?}")).or_insert(0_i64);
        *held = held.saturating_add(1);
    }
    counted
}

#[test]
fn a_cell_is_asked_of_a_position_at_a_level_that_exists() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let area = "geometry { type: 'Polygon', coordinates: [[[0, 0], [1, 0], [1, 1], [0, 0]]] }";
        for (call, names) in [
            (format!("geo::cell({area}, 3)"), "position"),
            (format!("geo::cell({}, 33)", point(0.0, 0.0)), "0 to 32"),
            (format!("geo::cell({}, -1)", point(0.0, 0.0)), "0 to 32"),
            (format!("geo::cell({}, 'fine')", point(0.0, 0.0)), "0 to 32"),
            (format!("geo::cell({}, 2.5)", point(0.0, 0.0)), "0 to 32"),
        ] {
            let refusal = session
                .run(&format!("RETURN {call};"))
                .expect_err(&call)
                .to_string();
            assert!(refusal.contains("geo::cell"), "{call}: {refusal}");
            assert!(refusal.contains(names), "{call}: {refusal}");
        }
        // Level 0 is the whole world, and it draws as the whole world.
        let Outcome::Value(Value::Geometry(Geometry::Polygon(world))) = run(
            &mut session,
            &format!("RETURN geo::cell({}, 0);", point(12.0, 34.0)),
        ) else {
            panic!("a cell is a polygon")
        };
        let corners: Vec<(f64, f64)> = world
            .exterior
            .0
            .iter()
            .map(|corner| (corner.longitude.round(), corner.latitude.round()))
            .collect();
        assert_eq!(
            corners,
            [
                (-180.0, -90.0),
                (180.0, -90.0),
                (180.0, 90.0),
                (-180.0, 90.0),
                (-180.0, -90.0)
            ],
            "{}",
            backend.name
        );
    });
}
