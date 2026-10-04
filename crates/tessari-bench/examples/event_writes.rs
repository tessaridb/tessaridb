//! What one event costs the write it runs for (G058 C4, Q-912).
//!
//! `cargo run --release -p tessari-bench --example event_writes`
//!
//! In memory: 5 000 single-record writes into a collection with no event, and
//! 5 000 into one whose `ON CREATE` event writes an audit record, seven rounds
//! each; prints the median per-write time of each and the audit records the
//! evented round left, so two builds can be compared on cost and shown to run
//! the same body.

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const WRITES: u32 = 5_000;
const ROUNDS: u32 = 7;

fn round(table: &str) -> Result<(f64, usize), Box<dyn Error>> {
    let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::new());
    let store = Store::open(backend)?;
    let mut session = Session::new(&store);
    session.run(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench; DEFINE DATABASE e; USE DATABASE e; \
         DEFINE COLLECTION plain; DEFINE COLLECTION orders; DEFINE COLLECTION audit; \
         DEFINE EVENT noted ON orders FOR CREATE THEN { \
           CREATE audit = { order: $id, kind: $event, total: $after.total }; };",
    )?;
    let began = Instant::now();
    for n in 0..WRITES {
        session.run(&format!("CREATE {table}:{n} = {{ total: {n} }};"))?;
    }
    let spent = began.elapsed().as_secs_f64() * 1e6 / f64::from(WRITES);
    let rows = session.run("SELECT * FROM audit;")?;
    let audited = rows
        .last()
        .and_then(|outcome| outcome.records())
        .map_or(0, <[_]>::len);
    Ok((spent, audited))
}

fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    times.get(times.len() / 2).copied().unwrap_or(f64::NAN)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut plain = Vec::new();
    let mut evented = Vec::new();
    let mut audited = 0;
    for _ in 0..ROUNDS {
        plain.push(round("plain")?.0);
        let (spent, left) = round("orders")?;
        evented.push(spent);
        audited = left;
    }
    println!(
        "{WRITES} writes × {ROUNDS} rounds, memory, release: plain {:.1} µs/write | \
         one event {:.1} µs/write | audit records {audited}",
        median(plain),
        median(evented)
    );
    Ok(())
}
