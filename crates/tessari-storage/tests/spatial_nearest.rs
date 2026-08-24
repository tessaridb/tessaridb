//! The nearest-first walk: what it answers, and what it refuses to answer.
//!
//! # What the oracle is, and what it deliberately is not
//!
//! The answer is defined by measuring every record there is and sorting. That is
//! the definition the walk has to agree with, and it is computed here from the
//! records rather than from any entry the index wrote or any bound the walk
//! ranks by.
//!
//! In particular the oracle never calls `no_closer_than`. That function is the
//! new thing under test — a floor under the distance to a whole cell — and an
//! oracle keyed by it would break in step with a broken walk and agree with it
//! all the way to a green suite. It does call `distance`, because `distance` is
//! what "nearest" *means* here; it is a separate kernel with its own oracles in
//! `tessari-geo`, and there is no second definition of a geodesic to hold it to.
//!
//! # The failure this exists to catch
//!
//! A walk that stops early does not answer short — it answers with the **wrong
//! records**, in a plausible order, and every one of them really is near the
//! target. Nothing about the shape of the answer says anything went wrong, which
//! is why the comparison is against a full measurement rather than against a
//! property of the result.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_geo::Snapped;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{
    Catalog, IndexDefinition, IndexShape, RecordAddress, Store, TableShape, Transaction,
};
use tessari_types::{DatabaseId, Geometry, NamespaceId, Path, Position, RecordId, TableId, Value};

/// One degree, in grid units.
const DEGREE: i64 = 1_000_000_000;
/// Half the world in longitude, which is the whole of it in latitude.
const HALF_WORLD: i64 = 180 * DEGREE;

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

    fn at(&self, name: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.table,
            RecordId::from(name),
        )
    }

    fn begin(&self) -> Transaction<'_> {
        self.store.begin().unwrap()
    }

    /// Write one record holding one geometry.
    fn write(&self, name: &str, geometry: Geometry) {
        let record = Value::Object(
            [("at".to_owned(), Value::Geometry(geometry))]
                .into_iter()
                .collect(),
        );
        let mut transaction = self.store.begin().unwrap();
        transaction.put(
            self.at(name),
            tessari_encoding::encode_payload(&record).into_bytes(),
        );
        transaction.commit().unwrap();
    }

    fn write_point(&self, name: &str, longitude: i64, latitude: i64) {
        self.write(
            name,
            Geometry::Point(Position::new(degrees(longitude), degrees(latitude))),
        );
    }

    /// Every record, measured and sorted. The definition of the answer.
    fn by_hand(&self, target: Snapped, wanted: usize) -> Vec<RecordId> {
        let transaction = self.begin();
        let found = transaction
            .scan_table(self.namespace, self.database, self.table)
            .unwrap();
        let mut placed = Vec::new();
        for (id, payload) in found {
            let record = tessari_encoding::decode_payload(&payload).unwrap();
            let Some(Value::Geometry(Geometry::Point(position))) =
                Path::field("at").resolve(&record)
            else {
                continue;
            };
            let metres = tessari_geo::distance(target, Snapped::of(*position).unwrap()).unwrap();
            placed.push((metres, id));
        }
        placed.sort_by(|(one, left), (other, right)| {
            one.total_cmp(other).then_with(|| left.cmp(right))
        });
        // The tie group at the bound belongs to the answer, so the cut is at the
        // distance of the last wanted record rather than at its position.
        let edge = placed
            .get(wanted.saturating_sub(1))
            .map_or(f64::INFINITY, |(metres, _)| *metres);
        placed
            .into_iter()
            .filter(|(metres, _)| *metres <= edge)
            .map(|(_, id)| id)
            .collect()
    }

    /// How far the nearest record is, measured by hand.
    ///
    /// Used to assert that the corpus is actually **around** the target. A
    /// generator can draw the scale it was asked for and still put every record
    /// somewhere else, and a pruning measurement over an empty neighbourhood
    /// reports the cost of finding nothing.
    fn nearest_metres(&self, target: Snapped) -> f64 {
        let transaction = self.begin();
        transaction
            .scan_table(self.namespace, self.database, self.table)
            .unwrap()
            .into_iter()
            .filter_map(|(_, payload)| {
                let record = tessari_encoding::decode_payload(&payload).unwrap();
                let Some(Value::Geometry(Geometry::Point(position))) =
                    Path::field("at").resolve(&record)
                else {
                    return None;
                };
                tessari_geo::distance(target, Snapped::of(*position).unwrap())
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// What the walk answers, with what it cost.
    fn by_index(&self, target: Snapped, wanted: usize) -> Option<(Vec<RecordId>, usize, usize)> {
        let transaction = self.begin();
        let nearby = transaction
            .records_by_place(&self.index, target, wanted)
            .unwrap()?;
        Some((
            nearby.rows.into_iter().map(|(id, _)| id).collect(),
            nearby.entries,
            nearby.expanded,
        ))
    }
}

/// Grid units as degrees, exactly.
///
/// Through a thousandth of a degree rather than straight to a double: a whole
/// grid-unit count fits an `i32` at that resolution and `f64::from` on an `i32`
/// loses nothing. A fixture that drifts by a unit is a fixture that can fail for
/// a reason that has nothing to do with the assertion.
fn degrees(units: i64) -> f64 {
    f64::from(i32::try_from(units / 1_000).unwrap_or(i32::MAX)) / 1_000_000.0
}

fn at(longitude: i64, latitude: i64) -> Snapped {
    Snapped::from_units(longitude, latitude).unwrap()
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

    /// A position within `reach` grid units of `middle`, on both axes.
    ///
    /// Drawn in **thousandths of a degree** and scaled up, which is not tidiness.
    /// [`Rolls::next`] yields thirty-one bits, so a draw taken directly in grid
    /// units cannot exceed about two degrees: ask for thirty and every record
    /// lands in a two-degree square at the low corner of the range, twenty-eight
    /// degrees from where the caller put the target. The corpus still looks
    /// random, the test still passes, and it measures a query against a store
    /// that is empty everywhere the query looks.
    fn near(&mut self, middle: Snapped, reach: i64) -> (i64, i64) {
        const MILLI: i64 = 1_000_000;
        let steps = reach.saturating_mul(2).checked_div(MILLI).unwrap_or(1);
        let span = u64::try_from(steps).unwrap_or(1);
        let mut drawn = |middle: i64| {
            middle
                .saturating_add(self.upto(span).saturating_mul(MILLI))
                .saturating_sub(reach)
        };
        (
            drawn(middle.longitude_units()).clamp(-HALF_WORLD, HALF_WORLD),
            drawn(middle.latitude_units()).clamp(-HALF_WORLD / 2, HALF_WORLD / 2),
        )
    }
}

#[test]
fn the_walk_answers_what_measuring_every_record_answers() {
    // The whole claim, over four bounds and eight targets against a corpus
    // scattered across a continent. Equality of the **ordered** lists: a walk
    // that stops early answers with the wrong records rather than with fewer,
    // and a walk that ranks wrongly answers with the right records in the wrong
    // order — neither shows up in a set comparison.
    let fixture = Fixture::new();
    let mut rolls = Rolls(0x9e37_79b9_7f4a_7c15);
    let middle = at(10 * DEGREE, 45 * DEGREE);
    for number in 0..120 {
        let (longitude, latitude) = rolls.near(middle, 20 * DEGREE);
        fixture.write_point(&format!("p{number}"), longitude, latitude);
    }
    for step in 0..8 {
        let (longitude, latitude) = rolls.near(middle, 25 * DEGREE);
        let target = at(longitude, latitude);
        for wanted in [1_usize, 3, 10, 25] {
            let (found, _, _) = fixture
                .by_index(target, wanted)
                .unwrap_or_else(|| panic!("the index should serve step {step} for {wanted}"));
            assert_eq!(
                found,
                fixture.by_hand(target, wanted),
                "step {step}, wanted {wanted}, target {target:?}"
            );
        }
    }
}

#[test]
fn a_record_further_than_the_bound_is_still_ranked_against_it() {
    // A near cluster and one outlier, with the bound wide enough to reach the
    // outlier. The frontier has to keep opening cells long after it has filled
    // the bound with the cluster, because "filled" is not "settled" — a cell
    // whose floor is below the worst answer held can still improve on it.
    let fixture = Fixture::new();
    for number in 0..4 {
        fixture.write_point(&format!("near{number}"), number * DEGREE / 100, 50 * DEGREE);
    }
    fixture.write_point("far", 80 * DEGREE, -20 * DEGREE);
    let target = at(0, 50 * DEGREE);
    let (found, _, _) = fixture.by_index(target, 5).unwrap();
    assert_eq!(found, fixture.by_hand(target, 5));
    assert_eq!(found.last(), Some(&RecordId::from("far")));
}

#[test]
fn the_tie_group_at_the_bound_travels_with_the_answer() {
    // Two records the same distance away and a bound of **one**. A walk that cut
    // at the count would answer with whichever of the two its frontier happened
    // to reach first, and which one that is would be an accident of curve order.
    // A scan sorts both to the same place and the bound keeps them both, so this
    // does.
    //
    // The tie is made by symmetry about the meridian through the target rather
    // than by two shapes that look about equal: east and west of a point on the
    // equator are the same distance from it as a matter of the ellipsoid, and
    // "about equal" would be a fixture that tests floating point instead.
    let fixture = Fixture::new();
    let reach = DEGREE / 2;
    fixture.write_point("east", reach, 0);
    fixture.write_point("west", -reach, 0);
    // A third, definitively further, so the tie group is a group and not the
    // whole store.
    fixture.write_point("beyond", 40 * DEGREE, 0);

    let target = at(0, 0);
    let (found, _, _) = fixture.by_index(target, 1).unwrap();
    assert_eq!(found, fixture.by_hand(target, 1));
    assert_eq!(found.len(), 2, "both ties belong to the answer: {found:?}");
    assert!(!found.contains(&RecordId::from("beyond")));
}

#[test]
fn a_record_that_is_not_a_position_gives_the_read_up() {
    // `geo::distance` takes positions, so a record holding a shape is an error in
    // the statement — which the scan reports and a walk cannot. Answering the
    // other records in distance order would be an index hiding a mistake, so the
    // walk refuses and the caller scans.
    let fixture = Fixture::new();
    fixture.write_point("a", 0, 0);
    fixture.write_point("b", DEGREE, 0);
    fixture.write(
        "an area",
        Geometry::Line(vec![Position::new(0.5, 0.5), Position::new(0.6, 0.6)]),
    );
    assert!(
        fixture.by_index(at(0, 0), 3).is_none(),
        "a stored box with extent is not a position and must not be ranked"
    );
}

#[test]
fn a_shape_at_a_coarse_cell_is_seen_even_when_the_walk_is_descending() {
    // The case above is too small to prove what it looks like it proves: three
    // records fit in one subtree read, so the walk meets the area without ever
    // splitting a cell. Descending is where it could miss one.
    //
    // A record's covering sits at cells the size of the record, so a continent
    // occupies **coarse** cells — the very cells a descent passes *through*
    // rather than into. Reading only what lies below the cell it is standing on
    // would step straight over it, and the read would then answer a tidy ten
    // nearest while the scan reported the caller's mistake. That is an index
    // changing what a read answers, which is the one thing an index may not do.
    let fixture = Fixture::new();
    let mut rolls = Rolls(0xc0a5_e11a_7c0a_11d5);
    let middle = at(10 * DEGREE, 45 * DEGREE);
    // Enough that the walk has to split rather than read the world outright.
    for number in 0..400 {
        let (longitude, latitude) = rolls.near(middle, 5 * DEGREE);
        fixture.write_point(&format!("p{number}"), longitude, latitude);
    }
    fixture.write(
        "a continent",
        Geometry::Line(vec![Position::new(-20.0, 30.0), Position::new(40.0, 70.0)]),
    );
    assert!(
        fixture.by_index(middle, 10).is_none(),
        "the shape sits above the descent and must still stop the read"
    );
}

#[test]
fn an_index_holding_fewer_records_than_the_bound_gives_the_read_up() {
    // The answer needs records the index does not hold — a record with no
    // geometry has no entry, and the value layer sorts an absence *last*, so a
    // short walk is exactly the case where the scan has more to say.
    let fixture = Fixture::new();
    fixture.write_point("a", 0, 0);
    fixture.write_point("b", DEGREE, 0);
    assert!(fixture.by_index(at(0, 0), 2).is_some());
    assert!(fixture.by_index(at(0, 0), 3).is_none());
}

#[test]
fn the_walk_reads_a_fraction_of_what_the_index_holds() {
    // The property worth asserting is not a ratio at one size — it is that the
    // cost does **not grow with the store**. A walk whose reads track the
    // population is a scan with extra steps, and would still look thrifty at any
    // single population picked to flatter it.
    //
    // A small store is deliberately read whole: `SPATIAL_WALK_SUBTREE_ENTRIES`
    // says a subtree of that size is taken in one scan rather than descended
    // into, and thirty-two levels of seeks to avoid reading two hundred entries
    // is a bad trade however elegant it looks. So the claim is about the slope,
    // and it is measured at three sizes an order apart.
    let mut readings = Vec::new();
    for population in [200_usize, 2_000, 20_000] {
        let fixture = Fixture::new();
        let mut rolls = Rolls(0x5ca1_e50d_0e5c_3d00);
        let middle = at(10 * DEGREE, 45 * DEGREE);
        for number in 0..population {
            let (longitude, latitude) = rolls.near(middle, 30 * DEGREE);
            fixture.write_point(&format!("p{number}"), longitude, latitude);
        }
        // The corpus has to be around the target for any of this to mean
        // anything: a walk over an empty neighbourhood measures the cost of
        // finding nothing and reports it as thrift. The threshold is derived
        // from the density rather than picked — records scattered over a square
        // of side `S` sit about `S/√n` apart — so it stays honest as the
        // population moves instead of being a number that happened to pass.
        let spread = f64::from(u32::try_from(population).unwrap_or(u32::MAX)).sqrt();
        let spacing = 60.0 / spread * 111_000.0;
        let nearest = fixture.nearest_metres(middle);
        assert!(
            nearest < spacing * 3.0,
            "at {population} records the nearest is {nearest:.0} m away against \
             a spacing of {spacing:.0} m — the corpus is not where the query looks"
        );
        let (found, entries, expanded) = fixture.by_index(middle, 10).unwrap();
        assert_eq!(found.len(), 10);
        readings.push((population, entries, expanded));
    }
    for (population, entries, expanded) in &readings {
        println!("population {population} | entries {entries} | cells {expanded}");
    }
    let (small, cheap, _) = readings[0];
    let (large, dear, cells) = readings[2];
    assert!(
        dear < cheap.saturating_mul(3),
        "the walk read {cheap} entries at {small} records and {dear} at {large} \
         in {cells} cells — the cost is tracking the store, which is a scan"
    );
    assert!(
        dear.saturating_mul(10) < large,
        "at {large} records the walk still read {dear} entries in {cells} cells"
    );
}
