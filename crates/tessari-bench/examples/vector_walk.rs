//! What one approximate vector read costs on a graph of 20 000 × 32-d (G058 C2).
//!
//! `cargo run --release -p tessari-bench --example vector_walk -- <empty dir>`
//!
//! Builds the index on disk through the language, then runs 200 queries — each
//! once cold and five times warm — as `ORDER BY vector::cosine(e, q) LIMIT 10
//! APPROXIMATE`, and prints the warm nearest-rank p50 and p99 with a fingerprint
//! of every answer, so two builds can be compared on cost and shown to answer
//! the same records.

use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use tessaridb::Db;

const RECORDS: usize = 20_000;
const WIDTH: usize = 32;
const QUERIES: usize = 200;
const REPEATS: usize = 5;

struct Rolls(u64);

impl Rolls {
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from(u32::try_from(self.0 >> 40).unwrap_or(0)) / f64::from(1_u32 << 24)
    }

    fn vector(&mut self) -> String {
        let parts: Vec<String> = (0..WIDTH)
            .map(|_| format!("{:.4}", self.unit() - 0.5))
            .collect();
        format!("[{}]", parts.join(", "))
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: vector_walk <empty directory>")?;
    let db = Db::open(&dir)?;
    let mut session = db.session();
    session.run(&format!(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench; DEFINE DATABASE v; USE DATABASE v; \
         DEFINE COLLECTION notes; DEFINE FIELD e ON notes TYPE vector<{WIDTH}>; \
         DEFINE INDEX by_e ON notes FIELDS e VECTOR cosine;"
    ))?;
    let mut rolls = Rolls(0x7665_6374_6f72_7321);
    let building = Instant::now();
    for chunk in 0..RECORDS.div_ceil(500) {
        let mut script = String::from("BEGIN;");
        for at in 0..500 {
            let id = chunk.saturating_mul(500).saturating_add(at);
            if id >= RECORDS {
                break;
            }
            script.push_str(&format!(
                " CREATE notes:{id} = {{ e: {} }};",
                rolls.vector()
            ));
        }
        script.push_str(" COMMIT;");
        session.run(&script)?;
    }
    println!(
        "built {RECORDS} × {WIDTH} in {:.1} s (disk, release)",
        building.elapsed().as_secs_f64()
    );
    let mut timings = Vec::new();
    let mut fingerprint = DefaultHasher::new();
    for _ in 0..QUERIES {
        let read = format!(
            "SELECT id FROM notes ORDER BY vector::cosine(e, {}) LIMIT 10 APPROXIMATE;",
            rolls.vector()
        );
        for repeat in 0..=REPEATS {
            let began = Instant::now();
            let outcomes = session.run(&read)?;
            let spent = began.elapsed();
            if repeat == 0 {
                let ids: Vec<String> = outcomes
                    .last()
                    .and_then(|outcome| outcome.records())
                    .map(|records| records.iter().map(|(id, _)| id.to_string()).collect())
                    .unwrap_or_default();
                ids.hash(&mut fingerprint);
            } else {
                timings.push(spent.as_micros());
            }
        }
    }
    timings.sort_unstable();
    let rank = |percent: usize| {
        let at = timings.len().saturating_mul(percent).div_ceil(100);
        timings.get(at.saturating_sub(1)).copied().unwrap_or(0)
    };
    println!(
        "approximate LIMIT 10: warm p50 {} µs | p99 {} µs | {} runs | answers {:016x}",
        rank(50),
        rank(99),
        timings.len(),
        fingerprint.finish()
    );
    Ok(())
}
