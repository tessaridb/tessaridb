//! How finely a query box should be covered: a measured sweep (G058 C1).
//!
//! `cargo run --release -p tessari-bench --example covering -- <empty dir>`
//!
//! A skewed corpus on disk — three dense cities, a sparse spread over a
//! continent, small buildings, roads and a few large regions — and three sizes
//! of query box drawn around stored records, each read through the spatial
//! index at a range of query-covering budgets. For every budget it reports the
//! index entries read, the candidates the boxes could not rule out, the records
//! that truly meet the query (the same at every budget — the control), and the
//! latency of the whole read, filter and refine together, at the nearest-rank
//! p50 and p99 over warm repeats.
//!
//! The corpus is generated from a fixed seed, so two runs on one machine read
//! the same records with the same queries.

use std::error::Error;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Instant;

use tessari_geo::{Bounds, Cell, Relation, Shape, Snapped, covering, intersects};
use tessari_kv::KvBackend;
use tessari_lsm::{LsmBackend, StoreConfig};
use tessari_storage::{Catalog, IndexDefinition, IndexShape, RecordAddress, Store, TableShape};
use tessari_types::{Geometry, Path, Polygon, Position, RecordId, Ring, Value};

const BUDGETS: [usize; 7] = [4, 8, 16, 32, 64, 128, 256];
const QUERIES_PER_SIZE: usize = 120;
const REPEATS: usize = 5;

/// A small linear-congruential generator: deterministic, and enough to scatter.
struct Rolls(u64);

impl Rolls {
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from(u32::try_from(self.0 >> 40).unwrap_or(0)) / f64::from(1_u32 << 24)
    }

    /// Roughly normal, by the sum of twelve uniforms.
    fn normal(&mut self) -> f64 {
        (0..12).map(|_| self.unit()).sum::<f64>() - 6.0
    }
}

fn square(longitude: f64, latitude: f64, side: f64) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior: Ring(vec![
            Position::new(longitude, latitude),
            Position::new(longitude + side, latitude),
            Position::new(longitude + side, latitude + side),
            Position::new(longitude, latitude + side),
            Position::new(longitude, latitude),
        ]),
        interiors: Vec::new(),
    })
}

fn corpus() -> Vec<Geometry> {
    let mut rolls = Rolls(0x636f_7665_7269_6e67);
    let cities = [(2.35, 48.86), (13.40, 52.52), (-3.70, 40.42)];
    let mut shapes = Vec::new();
    for _ in 0..30_000 {
        let (longitude, latitude) = cities[shapes.len() % 3];
        shapes.push(Geometry::Point(Position::new(
            longitude + rolls.normal() * 0.04,
            latitude + rolls.normal() * 0.03,
        )));
    }
    for _ in 0..10_000 {
        shapes.push(Geometry::Point(Position::new(
            -10.0 + rolls.unit() * 40.0,
            36.0 + rolls.unit() * 24.0,
        )));
    }
    for _ in 0..3_000 {
        let (longitude, latitude) = cities[shapes.len() % 3];
        shapes.push(square(
            longitude + rolls.normal() * 0.03,
            latitude + rolls.normal() * 0.02,
            0.0005,
        ));
    }
    for _ in 0..600 {
        let (longitude, latitude) = cities[shapes.len() % 3];
        let (from_x, from_y) = (
            longitude + rolls.normal() * 0.05,
            latitude + rolls.normal() * 0.04,
        );
        let length = 0.01 + rolls.unit() * 0.05;
        shapes.push(Geometry::Line(vec![
            Position::new(from_x, from_y),
            Position::new(from_x + length, from_y + length * (rolls.unit() - 0.5)),
        ]));
    }
    for _ in 0..40 {
        shapes.push(square(
            -10.0 + rolls.unit() * 38.0,
            36.0 + rolls.unit() * 22.0,
            0.5 + rolls.unit() * 1.5,
        ));
    }
    shapes
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: covering <empty directory>")?;
    if FsPath::new(&dir).exists() && std::fs::read_dir(&dir)?.next().is_some() {
        return Err("the directory must be empty".into());
    }
    let backend: Arc<dyn KvBackend> = Arc::new(LsmBackend::open(&dir, StoreConfig::default())?);
    let store = Store::open(backend)?;
    let (index, home) = defined(&store)?;
    let shapes = corpus();
    let loading = Instant::now();
    for (chunk, slice) in shapes.chunks(2_000).enumerate() {
        let mut transaction = store.begin()?;
        for (at, geometry) in slice.iter().enumerate() {
            let record = Value::Object(
                [("at".to_owned(), Value::Geometry(geometry.clone()))]
                    .into_iter()
                    .collect(),
            );
            let id = chunk.saturating_mul(2_000).saturating_add(at);
            transaction.put(
                home(RecordId::from(format!("r{id}").as_str())),
                tessari_encoding::encode_payload(&record).into_bytes(),
            );
        }
        transaction.commit()?;
    }
    println!(
        "corpus: {} records (30 000 city points, 10 000 spread, 3 000 buildings, 600 roads, \
         40 regions) loaded in {:.1} s; disk store, release build",
        shapes.len(),
        loading.elapsed().as_secs_f64()
    );

    let decoded: Vec<Shape> = shapes.iter().map(Shape::of).collect::<Result<_, _>>()?;
    let mut rolls = Rolls(0x7175_6572_7920_6278);
    for (size, half) in [("street", 0.004), ("district", 0.05), ("country", 1.5)] {
        let queries: Vec<Bounds> = (0..QUERIES_PER_SIZE)
            .map(|_| {
                let pick = usize::try_from(rolls.0 >> 33)
                    .unwrap_or(0)
                    .checked_rem(shapes.len())
                    .unwrap_or(0);
                rolls.unit();
                let centre = decoded[pick]
                    .bounds()
                    .map_or(Position::new(0.0, 0.0), |held| {
                        Position::new(
                            (degrees(held.west()) + degrees(held.east())) / 2.0,
                            (degrees(held.south()) + degrees(held.north())) / 2.0,
                        )
                    });
                boxed(centre, half)
            })
            .collect::<Result<_, _>>()?;
        for budget in BUDGETS {
            let mut timings = Vec::new();
            let (mut entries, mut candidates, mut results) = (0_usize, 0_usize, 0_usize);
            for (number, query) in queries.iter().enumerate() {
                let cells: Vec<Cell> = covering(*query, budget)
                    .into_iter()
                    .map(|(cell, _)| cell)
                    .collect();
                let probe = Shape::of(&query_polygon(*query))?;
                for repeat in 0..=REPEATS {
                    let began = Instant::now();
                    let transaction = store.begin()?;
                    let region =
                        transaction.records_in_region(&index, &cells, *query, Relation::Meets)?;
                    let mut met = 0_usize;
                    for (_, payload) in &region.rows {
                        let record = tessari_encoding::decode_payload(payload)?;
                        if let Some(Value::Geometry(held)) = Path::field("at").resolve(&record)
                            && intersects(&Shape::of(held)?, &probe)
                        {
                            met = met.saturating_add(1);
                        }
                    }
                    let spent = began.elapsed();
                    if repeat > 0 {
                        timings.push(spent.as_secs_f64() * 1e3);
                    }
                    if repeat == 0 && number < QUERIES_PER_SIZE {
                        entries = entries.saturating_add(region.entries);
                        candidates = candidates.saturating_add(region.candidates);
                        results = results.saturating_add(met);
                    }
                }
            }
            timings.sort_by(f64::total_cmp);
            // Nearest rank, in whole percent so no float becomes an index.
            let rank = |percent: usize| {
                let at = timings.len().saturating_mul(percent).div_ceil(100);
                timings
                    .get(at.saturating_sub(1))
                    .copied()
                    .unwrap_or(f64::NAN)
            };
            println!(
                "{size:8} budget {budget:3} | entries {entries:8} | candidates {candidates:8} | \
                 results {results:7} | p50 {:.3} ms | p99 {:.3} ms",
                rank(50),
                rank(99)
            );
        }
    }
    Ok(())
}

type Home = Box<dyn Fn(RecordId) -> RecordAddress>;

fn defined(store: &Store) -> Result<(IndexDefinition, Home), Box<dyn Error>> {
    let mut transaction = store.begin()?;
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("atlas")?.id;
    let database = catalog.create_database(namespace, "world")?.id;
    let table = catalog
        .create_table(namespace, database, "places", TableShape::default())?
        .id;
    let index = catalog.create_index(
        table,
        "by_at",
        vec![Path::field("at")],
        IndexShape {
            spatial: true,
            containment: false,
            ..IndexShape::default()
        },
    )?;
    transaction.commit()?;
    Ok((
        index,
        Box::new(move |id| RecordAddress::new(namespace, database, table, id)),
    ))
}

fn boxed(centre: Position, half: f64) -> Result<Bounds, Box<dyn Error>> {
    let corner = |longitude: f64, latitude: f64| {
        Snapped::of(Position::new(
            longitude.clamp(-180.0, 180.0),
            latitude.clamp(-90.0, 90.0),
        ))
    };
    let low = corner(centre.longitude - half, centre.latitude - half)?;
    let high = corner(centre.longitude + half, centre.latitude + half)?;
    Bounds::of_positions(&[low, high]).ok_or_else(|| "an empty query".into())
}

fn query_polygon(query: Bounds) -> Geometry {
    let (west, south) = (degrees(query.west()), degrees(query.south()));
    let (east, north) = (degrees(query.east()), degrees(query.north()));
    square(west, south, (east - west).max(north - south))
}

fn degrees(units: i64) -> f64 {
    f64::from(i32::try_from(units / 1_000).unwrap_or(0)) / 1_000_000.0
}
