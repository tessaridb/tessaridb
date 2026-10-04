//! What keeping a grouped materialized view costs per batch (G058 C4, Q-908).
//!
//! `cargo run --release -p tessari-bench --example view_groups -- <empty dir>`
//!
//! 50 000 records in 500 groups on disk, a view of `count`, `sum` and `max` per
//! group, then 100 batches that each change one record and bring the view
//! current. Prints the nearest-rank p50 and p99 of the maintenance pass and
//! whether the view then equals its read, so two builds can be compared on cost
//! and shown to keep the same rows.

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use tessari_kv::KvBackend;
use tessari_lsm::{LsmBackend, StoreConfig};
use tessari_session::{Session, maintain_views};
use tessari_storage::Store;

const RECORDS: u64 = 50_000;
const GROUPS: u64 = 500;
const BATCHES: u64 = 100;
const READ: &str = "SELECT g, count(*) AS c, sum(v) AS s, max(v) AS hi FROM t GROUP BY g";

fn rows(
    session: &mut Session<'_>,
    script: &str,
) -> Result<Vec<tessari_types::Value>, Box<dyn Error>> {
    let outcomes = session.run(script)?;
    Ok(outcomes
        .last()
        .and_then(|outcome| outcome.records())
        .map(|records| records.iter().map(|(_, row)| row.clone()).collect())
        .unwrap_or_default())
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: view_groups <empty directory>")?;
    let backend: Arc<dyn KvBackend> = Arc::new(LsmBackend::open(&dir, StoreConfig::default())?);
    let store = Store::open(backend)?;
    let mut session = Session::new(&store);
    session.run(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench; DEFINE DATABASE v; USE DATABASE v; \
         DEFINE COLLECTION t;",
    )?;
    for chunk in 0..RECORDS.div_ceil(1_000) {
        let mut script = String::from("BEGIN;");
        for at in 0..1_000 {
            let n = chunk.saturating_mul(1_000).saturating_add(at);
            script.push_str(&format!(
                " CREATE t:{n} = {{ g: {}, v: {} }};",
                n % GROUPS,
                n.wrapping_mul(7_919) % 1_000
            ));
        }
        script.push_str(" COMMIT;");
        session.run(&script)?;
    }
    let defining = Instant::now();
    session.run(&format!("DEFINE VIEW kept MATERIALIZED AS {READ};"))?;
    println!(
        "{RECORDS} records in {GROUPS} groups; view defined in {:.1} ms (disk, release)",
        defining.elapsed().as_secs_f64() * 1e3
    );
    let mut timings = Vec::new();
    for batch in 0..BATCHES {
        let n = batch.wrapping_mul(4_099) % RECORDS;
        session.run(&format!("UPDATE t:{n} SET v = {};", batch % 1_000))?;
        let began = Instant::now();
        maintain_views(&store)?;
        timings.push(began.elapsed().as_micros());
    }
    timings.sort_unstable();
    let rank = |percent: usize| {
        let at = timings.len().saturating_mul(percent).div_ceil(100);
        timings.get(at.saturating_sub(1)).copied().unwrap_or(0)
    };
    let kept = rows(&mut session, "SELECT * FROM kept;")?;
    let read = rows(&mut session, &format!("{READ};"))?;
    println!(
        "one change per batch: maintain p50 {} µs | p99 {} µs | {} batches | equals its read: {}",
        rank(50),
        rank(99),
        timings.len(),
        kept == read
    );
    Ok(())
}
