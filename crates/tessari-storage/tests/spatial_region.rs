//! The filter half of a spatial read: what the traversal reaches, and what it
//! must not miss.
//!
//! # Why this is a storage test and not a query test
//!
//! A query re-tests every candidate against the real geometry, so a *surplus*
//! candidate is invisible from above — the answer is right and only the cost is
//! wrong. The failure that matters runs the other way: a record the traversal
//! never reaches is a row that silently is not in the answer, and from above
//! that is indistinguishable from a query that legitimately matched less.
//!
//! So the properties are asserted here, where the candidate set itself can be
//! looked at, and against an **oracle** — every record whose box the relation
//! admits, computed by walking every record there is. That is the definition the
//! index has to agree with, and comparing the index against itself would prove
//! nothing.
//!
//! # The scale is a parameter, twice over
//!
//! Query boxes are drawn at several sizes, from a few metres to a continent, and
//! records likewise. A generator that only produces one scale certifies defects
//! at every other scale, which this project has now been bitten by twice: a
//! covering test that only used world-scale boxes, and an ordering test that
//! only placed entries at the finest level. Both passed with the thing they
//! existed to catch broken.
//!
//! The record scale is the load-bearing one here. A record **larger** than the
//! query box sits at a coarser cell, whose range begins below the query cell's
//! own, so no forward scan reaches it — it is found only by the ancestor
//! lookups. Drop those and every assertion about small records still passes.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_constants::{SPATIAL_INDEX_CELLS_PER_RECORD, SPATIAL_QUERY_CELLS};
use tessari_geo::{Bounds, Relation, Snapped};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{
    Catalog, IndexDefinition, IndexShape, RecordAddress, Store, TableShape, Transaction,
};
use tessari_types::{DatabaseId, Geometry, NamespaceId, Path, Position, RecordId, TableId, Value};

/// One degree, in grid units.
const DEGREE: i64 = 1_000_000_000;
/// Half the world in longitude, which is the whole of it in latitude.
const HALF_WORLD: i64 = 180 * DEGREE;
/// The full span of longitude.
const WHOLE_WORLD: i64 = 360 * DEGREE;

struct Fixture {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    index: IndexDefinition,
}

impl Fixture {
    fn new() -> Self {
        let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
        let store = Store::open(backend).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("atlas").unwrap();
        let database = catalog.create_database(namespace.id, "world").unwrap();
        let table = catalog
            .create_table(namespace.id, database.id, "places", TableShape::default())
            .unwrap();
        let index = catalog
            .create_index(
                table.id,
                "by_where",
                vec![Path::field("at")],
                IndexShape {
                    unique: false,
                    search: false,
                    spatial: true,
                    vector: None,
                },
            )
            .unwrap();
        transaction.commit().unwrap();
        Self {
            store,
            namespace: namespace.id,
            database: database.id,
            table: table.id,
            index,
        }
    }

    /// Write one rectangle under the given name.
    fn write(&self, name: &str, corners: Bounds) {
        let mut transaction = self.store.begin().unwrap();
        transaction.put(self.at(name), payload(corners));
        transaction.commit().unwrap();
    }

    fn at(&self, name: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(name),
        )
    }

    /// The box a record actually ended up with, read back from the store.
    ///
    /// Coordinates travel through degrees, so a rectangle stated in grid units
    /// does not always come back as the same units. Anything asserting about an
    /// **exact** box has to ask the store what it stored rather than assume.
    fn box_of(&self, name: &str) -> Bounds {
        let transaction = self.begin();
        let payload = transaction.get(&self.at(name)).unwrap().unwrap();
        let record = tessari_encoding::decode_payload(&payload).unwrap();
        let Some(Value::Geometry(geometry)) = Path::field("at").resolve(&record) else {
            panic!("the record should carry a geometry");
        };
        tessari_geo::Shape::of(geometry).unwrap().bounds().unwrap()
    }

    fn begin(&self) -> Transaction<'_> {
        self.store.begin().unwrap()
    }

    /// Every record whose stored box the relation admits, found by looking at
    /// every record there is.
    ///
    /// The oracle. It is what the index is required to agree with, and it is
    /// derived from the records rather than from any entry the index wrote.
    fn by_hand(&self, query: Bounds, relation: Relation) -> Vec<RecordId> {
        let transaction = self.begin();
        let found = transaction
            .scan_table(self.namespace, self.database, self.table)
            .unwrap();
        let mut kept = Vec::new();
        for (id, payload) in found {
            let record = tessari_encoding::decode_payload(&payload).unwrap();
            let Some(Value::Geometry(geometry)) = Path::field("at").resolve(&record) else {
                continue;
            };
            let bounds = tessari_geo::Shape::of(geometry).unwrap().bounds().unwrap();
            if admits(relation, query, bounds) {
                kept.push(id);
            }
        }
        kept.sort();
        kept
    }

    /// What the index offers for the same question.
    fn by_index(&self, query: Bounds, relation: Relation) -> (Vec<RecordId>, usize, usize) {
        let cells: Vec<tessari_geo::Cell> = tessari_geo::covering(query, SPATIAL_QUERY_CELLS)
            .into_iter()
            .map(|(cell, _)| cell)
            .collect();
        let transaction = self.begin();
        let region = transaction
            .records_in_region(&self.index, &cells, query, relation)
            .unwrap();
        let mut ids: Vec<RecordId> = region.rows.into_iter().map(|(id, _)| id).collect();
        ids.sort();
        (ids, region.entries, region.candidates)
    }
}

/// A shape whose box is exactly the given rectangle.
///
/// A two-position line across the diagonal rather than a ring. This test is
/// about the **filter**, and the filter sees only boxes — a polygon would add
/// ring-validity failure modes that have nothing to do with what is being
/// asserted, and would make a failure here ambiguous between the two.
fn shape(bounds: Bounds) -> Geometry {
    Geometry::Line(vec![
        Position::new(degrees(bounds.west()), degrees(bounds.south())),
        Position::new(degrees(bounds.east()), degrees(bounds.north())),
    ])
}

/// Grid units as degrees, the way a caller writes them.
///
/// Through a thousandth of a degree rather than straight to a double, so the
/// conversion is exact rather than nearly so: a whole grid unit count fits an
/// `i32` at that resolution, and `f64::from` on an `i32` loses nothing. A test
/// whose fixtures drift by a unit is a test that can fail for a reason that has
/// nothing to do with what it asserts.
fn degrees(units: i64) -> f64 {
    f64::from(i32::try_from(units / 1_000).unwrap_or(i32::MAX)) / 1_000_000.0
}

/// One record holding one shape.
fn payload(corners: Bounds) -> Vec<u8> {
    let record = Value::Object(
        [("at".to_owned(), Value::Geometry(shape(corners)))]
            .into_iter()
            .collect(),
    );
    tessari_encoding::encode_payload(&record).into_bytes()
}

fn at(longitude: i64, latitude: i64) -> Snapped {
    Snapped::from_units(longitude, latitude).unwrap()
}

fn span(west: i64, south: i64, east: i64, north: i64) -> Bounds {
    Bounds::of_position(at(west, south)).widened_to(at(east, north))
}

/// A deterministic sequence, so a failure is reproducible from its own seed.
struct Rolls(u64);

impl Rolls {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A value in `0..span`, or zero for an empty span.
    fn upto(&mut self, span: u64) -> i64 {
        let drawn = self.next().checked_rem(span.max(1)).unwrap_or(0);
        i64::try_from(drawn).unwrap_or(0)
    }

    /// A square box of the given side, placed so it stays on the planet.
    ///
    /// The placement is drawn from the room that is **left** after the side is
    /// taken, rather than drawn freely and clamped. Clamping would pile boxes up
    /// against the poles and the meridian, which is where the coverings are
    /// least like the rest of the world — the generator would then be quietly
    /// testing one corner case over and over instead of the spread it claims.
    fn somewhere(&mut self, side: i64) -> Bounds {
        let side = side.min(HALF_WORLD);
        let east_room = WHOLE_WORLD.saturating_sub(side) / DEGREE;
        let north_room = HALF_WORLD.saturating_sub(side) / DEGREE;
        let west = self
            .upto(u64::try_from(east_room).unwrap_or(0))
            .saturating_mul(DEGREE)
            .saturating_sub(HALF_WORLD);
        let south = self
            .upto(u64::try_from(north_room).unwrap_or(0))
            .saturating_mul(DEGREE)
            .saturating_sub(HALF_WORLD / 2);
        span(
            west,
            south,
            west.saturating_add(side),
            south.saturating_add(side),
        )
    }
}

#[test]
fn a_record_larger_than_the_query_is_still_reached() {
    // The ancestor lookups, stated as the smallest case that needs them. The
    // continent's covering sits at coarse cells; the query is a city block, so
    // its covering is fine and every one of its cells lies *inside* a continent
    // cell rather than below it. A forward scan from the query cells therefore
    // never passes the continent's entries at all.
    //
    // Delete the ancestor half of `records_in_region` and this is the only test
    // in the suite that notices.
    let fixture = Fixture::new();
    fixture.write(
        "continent",
        span(-20 * DEGREE, 30 * DEGREE, 40 * DEGREE, 70 * DEGREE),
    );
    fixture.write(
        "block",
        span(
            2 * DEGREE,
            48 * DEGREE,
            2 * DEGREE + 1_000_000,
            48 * DEGREE + 1_000_000,
        ),
    );

    let query = span(
        2 * DEGREE + 100_000,
        48 * DEGREE + 100_000,
        2 * DEGREE + 200_000,
        48 * DEGREE + 200_000,
    );
    let (found, _, _) = fixture.by_index(query, Relation::Meets);
    assert!(
        found.contains(&RecordId::from("continent")),
        "a record larger than the query box must still be reached; found {found:?}"
    );
    assert_eq!(found, fixture.by_hand(query, Relation::Meets));
}

#[test]
fn the_candidates_are_exactly_what_the_boxes_admit_at_every_scale() {
    // The whole filter, against the oracle, over four relations and a spread of
    // record and query sizes. Equality both ways: a missing candidate is a lost
    // row, and an extra one is a box test wider than the relation it stands for.
    let fixture = Fixture::new();
    let mut rolls = Rolls(0x0d0e_5c3d_5a71_a100);

    let sizes = [
        ("tiny", 1_000_000_i64),
        ("street", 20_000_000),
        ("city", 400_000_000),
        ("region", 8 * DEGREE),
        ("country", 30 * DEGREE),
    ];
    for (name, size) in sizes {
        for n in 0..6_u32 {
            fixture.write(&format!("{name}-{n}"), rolls.somewhere(size));
        }
    }

    // A query box equal to a record's own stored box, so `Same` has a case that
    // answers with something. Without one it would be satisfied by any filter
    // that returns nothing at all, which every broken filter also does.
    let identical = fixture.box_of("city-0");

    let mut answered = 0_usize;
    for relation in [
        Relation::Meets,
        Relation::Inside,
        Relation::Around,
        Relation::Same,
    ] {
        let mut boxes = vec![identical];
        for size in [
            1_000_000_i64,
            50_000_000,
            2 * DEGREE,
            25 * DEGREE,
            90 * DEGREE,
        ] {
            for _ in 0..8 {
                boxes.push(rolls.somewhere(size));
            }
        }
        for query in boxes {
            let (found, _, _) = fixture.by_index(query, relation);
            let expected = fixture.by_hand(query, relation);
            assert_eq!(
                found,
                expected,
                "the index disagreed with the records for {relation:?} over the box \
                 ({}, {}) to ({}, {})",
                query.west(),
                query.south(),
                query.east(),
                query.north()
            );
            answered = answered.saturating_add(found.len());
        }
    }
    assert!(
        answered > 0,
        "a filter that never answers with anything satisfies every assertion above"
    );
}

#[test]
fn a_record_written_and_not_committed_is_offered_whatever_its_place() {
    // It has no entry yet, so it has no box to be filtered by. Offering it
    // unconditionally costs one refinement; withholding it would hide a record
    // from the transaction that wrote it, which is the one thing snapshot
    // isolation promises not to do.
    let fixture = Fixture::new();
    fixture.write(
        "far",
        span(100 * DEGREE, 10 * DEGREE, 101 * DEGREE, 11 * DEGREE),
    );

    let mut transaction = fixture.begin();
    let pending = fixture.at("pending");
    transaction.put(pending, payload(span(0, 0, DEGREE, DEGREE)));

    let query = span(100 * DEGREE, 10 * DEGREE, 101 * DEGREE, 11 * DEGREE);
    let cells: Vec<tessari_geo::Cell> = tessari_geo::covering(query, SPATIAL_QUERY_CELLS)
        .into_iter()
        .map(|(cell, _)| cell)
        .collect();
    let region = transaction
        .records_in_region(&fixture.index, &cells, query, Relation::Meets)
        .unwrap();
    let ids: Vec<RecordId> = region.rows.into_iter().map(|(id, _)| id).collect();
    assert!(ids.contains(&RecordId::from("pending")));
    assert!(ids.contains(&RecordId::from("far")));
}

#[test]
fn the_counts_separate_the_traversal_from_what_survives_it() {
    // The health metric, asserted as an ordering rather than as a number: a
    // record has up to `SPATIAL_INDEX_CELLS_PER_RECORD` entries, so entries are
    // never fewer than the records they name, and the boxes can only reject.
    // Pinning actual values here would make the test a record of one covering
    // rather than a statement about the read.
    let fixture = Fixture::new();
    for n in 0..12_i64 {
        fixture.write(
            &format!("p{n}"),
            span(
                n * DEGREE,
                n * DEGREE,
                n * DEGREE + DEGREE / 2,
                n * DEGREE + DEGREE / 2,
            ),
        );
    }
    let query = span(0, 0, 4 * DEGREE, 4 * DEGREE);
    let cells: Vec<tessari_geo::Cell> = tessari_geo::covering(query, SPATIAL_QUERY_CELLS)
        .into_iter()
        .map(|(cell, _)| cell)
        .collect();
    let transaction = fixture.begin();
    let region = transaction
        .records_in_region(&fixture.index, &cells, query, Relation::Meets)
        .unwrap();

    assert!(region.reached > 0, "the traversal should reach something");
    assert!(
        region.entries >= region.reached,
        "a record is named by at least one entry: {} entries for {} records",
        region.entries,
        region.reached
    );
    assert!(
        region.entries <= region.reached * SPATIAL_INDEX_CELLS_PER_RECORD,
        "no record has more entries than the write budget allows"
    );
    assert!(
        region.candidates <= region.reached,
        "the box test can only reject"
    );
    assert_eq!(region.rows.len(), region.candidates);
}

#[test]
fn the_candidate_to_result_ratio_is_measured_at_every_scale() {
    // The health metric of the whole index. It is **reported** rather than
    // pinned to a threshold, because a threshold here would be a number nobody
    // measured — and the one thing that can be asserted without inventing one is
    // the property the filter must have: every true result is among the
    // candidates. A ratio is a cost; a missing result is a wrong answer.
    //
    // Two record shapes on purpose. A compact one, where the box approximates
    // the geometry well, and a long diagonal, where the box is many times the
    // shape's own extent — a river, a road, a border. The second is the case a
    // bounding-box index serves worst, and a harness that only drew the first
    // would report a ratio the store does not actually have.
    let fixture = Fixture::new();
    let mut rolls = Rolls(0x5ca1_e50d_0e5c_3d00);
    let mut placed = Vec::new();
    for n in 0..40_u32 {
        let corners = rolls.somewhere(200_000_000);
        fixture.write(&format!("small-{n}"), corners);
        placed.push(corners);
    }
    for n in 0..10_u32 {
        // Long and thin: a box spanning twenty degrees around a line that
        // occupies almost none of it.
        let corners = rolls.somewhere(20 * DEGREE);
        fixture.write(&format!("long-{n}"), corners);
        placed.push(corners);
    }

    println!("  side          entries  reached  candidates  results  ratio");
    let mut measured = 0_usize;
    for side in [
        10_000_000_i64,
        200_000_000,
        2 * DEGREE,
        10 * DEGREE,
        60 * DEGREE,
    ] {
        let mut entries = 0_usize;
        let mut reached = 0_usize;
        let mut candidates = 0_usize;
        let mut results = 0_usize;
        for turn in 0..24_usize {
            // Centred on a record rather than dropped anywhere on the planet.
            // Uniform placement over a world holding fifty shapes means the
            // small windows land on empty ocean every time, so the ratio at
            // exactly the scales where cell alignment matters is measured over
            // an answer of nothing — a harness reporting `inf` for the rows it
            // exists to report on. The scale being a parameter is not enough if
            // the world is too sparse for the parameter to reach anything.
            let anchor = placed
                .get(turn.wrapping_mul(7).wrapping_rem(placed.len()))
                .copied()
                .expect("the world holds records");
            let query = around(anchor, side);
            let cells: Vec<tessari_geo::Cell> = tessari_geo::covering(query, SPATIAL_QUERY_CELLS)
                .into_iter()
                .map(|(cell, _)| cell)
                .collect();
            let transaction = fixture.begin();
            let region = transaction
                .records_in_region(&fixture.index, &cells, query, Relation::Meets)
                .unwrap();
            entries = entries.saturating_add(region.entries);
            reached = reached.saturating_add(region.reached);
            candidates = candidates.saturating_add(region.candidates);

            // What the exact predicate keeps, computed from the geometries
            // themselves — the definition the filter has to be a superset of.
            let truth = truly_meeting(&fixture, query);
            results = results.saturating_add(truth.len());
            let offered: Vec<RecordId> = region.rows.into_iter().map(|(id, _)| id).collect();
            for id in &truth {
                assert!(
                    offered.contains(id),
                    "a record whose geometry meets the query must be among the \
                     candidates; `{id:?}` was not"
                );
            }
        }
        let ratio = ratio_of(candidates, results);
        println!(
            "  {side:>12}  {entries:>7}  {reached:>7}  {candidates:>10}  {results:>7}  {ratio:>5.1}"
        );
        // Per scale, not in total. A total lets four informative rows cover for
        // one that measured an empty world, which is how a harness comes to
        // report a ratio for a scale it never actually reached.
        assert!(
            results > 0,
            "no window of {side} units held a shape, so this row measured nothing"
        );
        assert!(
            candidates >= results,
            "the filter must offer at least what the predicate keeps"
        );
        measured = measured.saturating_add(results);
    }
    assert!(measured > 0);
}

/// Whether a relation admits a record's box — said again, from the corners.
///
/// Written out longhand rather than calling [`Relation::admits`], and the
/// falsification pass is why. The first version of the oracle called it, so
/// swapping `Inside` and `Around` inside the implementation broke the index and
/// the oracle **identically** and every assertion still held. A test comparing a
/// function against itself proves nothing, and this file's own header claims it
/// does not — so it has to actually not.
fn admits(relation: Relation, query: Bounds, record: Bounds) -> bool {
    match relation {
        Relation::Meets => {
            query.west() <= record.east()
                && record.west() <= query.east()
                && query.south() <= record.north()
                && record.south() <= query.north()
        }
        Relation::Inside => {
            query.west() <= record.west()
                && query.south() <= record.south()
                && record.east() <= query.east()
                && record.north() <= query.north()
        }
        Relation::Around => {
            record.west() <= query.west()
                && record.south() <= query.south()
                && query.east() <= record.east()
                && query.north() <= record.north()
        }
        Relation::Same => {
            query.west() == record.west()
                && query.south() == record.south()
                && query.east() == record.east()
                && query.north() == record.north()
        }
    }
}

/// A square of the given side, centred on another box, and kept on the planet.
fn around(anchor: Bounds, side: i64) -> Bounds {
    let half = side / 2;
    let middle_x = anchor.west().saturating_add(anchor.east()) / 2;
    let middle_y = anchor.south().saturating_add(anchor.north()) / 2;
    let west = middle_x
        .saturating_sub(half)
        .clamp(-HALF_WORLD, HALF_WORLD.saturating_sub(side));
    let south = middle_y
        .saturating_sub(half)
        .clamp(-HALF_WORLD / 2, (HALF_WORLD / 2).saturating_sub(side));
    span(
        west,
        south,
        west.saturating_add(side),
        south.saturating_add(side),
    )
}

/// Every record whose real geometry meets the query rectangle.
fn truly_meeting(fixture: &Fixture, query: Bounds) -> Vec<RecordId> {
    let window = tessari_geo::Shape::of(&shape_filling(query)).unwrap();
    let transaction = fixture.begin();
    let found = transaction
        .scan_table(fixture.namespace, fixture.database, fixture.table)
        .unwrap();
    let mut kept = Vec::new();
    for (id, payload) in found {
        let record = tessari_encoding::decode_payload(&payload).unwrap();
        let Some(Value::Geometry(geometry)) = Path::field("at").resolve(&record) else {
            continue;
        };
        if tessari_geo::intersects(&tessari_geo::Shape::of(geometry).unwrap(), &window) {
            kept.push(id);
        }
    }
    kept
}

/// The query rectangle as a closed ring — the shape a caller would actually
/// write, and the one the exact predicate has to be given.
fn shape_filling(bounds: Bounds) -> Geometry {
    let west = degrees(bounds.west());
    let south = degrees(bounds.south());
    let east = degrees(bounds.east());
    let north = degrees(bounds.north());
    Geometry::Polygon(tessari_types::Polygon {
        exterior: tessari_types::Ring(vec![
            Position::new(west, south),
            Position::new(east, south),
            Position::new(east, north),
            Position::new(west, north),
            Position::new(west, south),
        ]),
        interiors: Vec::new(),
    })
}

/// Candidates per result, as a number a person can read.
fn ratio_of(candidates: usize, results: usize) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a reported ratio over counts this size loses nothing a reader \
                  would notice"
    )]
    {
        candidates as f64 / results.max(1) as f64
    }
}
