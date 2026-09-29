//! Bytes per point of a series on disk, after a forced compaction (G044 C10).
//!
//! `cargo run --release -p tessari-bench --example series_bytes -- <dir> [points] [time]`
//!
//! Writes `points` readings — one a second, ten sensors, a float that wanders —
//! to a series in a fresh store at `<dir>`, closes it, reopens the directory as
//! the engine itself, compacts every region, and reports the SST bytes per
//! point. `time` declares the series `TIME at` (a build that has the clause);
//! without it the series is ordered by arrival, which every build has. The same
//! file is run against two builds, so the instrument is the same and only the
//! engine differs.

use std::error::Error;
use std::fs;
use std::path::Path;

use tessari_kv::{KeyRange, Keyspace, KvBackend, ScanRequest};
use tessari_lsm::{LsmBackend, StoreConfig};
use tessaridb::Db;

const BATCH: u64 = 10_000;

fn sst_bytes(dir: &Path) -> Result<u64, Box<dyn Error>> {
    let mut total = 0_u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "sst") {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    let dir = Path::new(args.get(1).ok_or("a store directory")?);
    let points: u64 = args.get(2).map_or(Ok(1_000_000), |held| held.parse())?;
    let time = args.get(3).is_some_and(|held| held == "time");
    let clause = if time { " TIME at" } else { "" };

    let db = Db::open(dir)?;
    let mut session = db.session();
    session.run(&format!(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench; DEFINE DATABASE bench; \
         USE DATABASE bench; DEFINE SERIES readings RETAIN 36500d{clause};"
    ))?;
    let start = 1_790_000_000_u64;
    let mut written = 0_u64;
    while written < points {
        let mut script = String::from("BEGIN;");
        let upto = written.saturating_add(BATCH).min(points);
        for n in written..upto {
            let at = start.saturating_add(n);
            let wander = f64::from(u32::try_from(n % 997).unwrap_or(0)) / 100.0;
            script.push_str(&format!(
                " CREATE readings = {{ sensor: 's{}', v: {:.2}, at: time::from_unix({at}) }};",
                n % 10,
                20.0 + wander
            ));
        }
        script.push_str(" COMMIT;");
        session.run(&script)?;
        written = upto;
    }
    drop(session);
    drop(db);

    let backend = LsmBackend::open(dir, StoreConfig::default())?;
    backend.compact()?;
    // What each region holds before the engine's own compression: the part a
    // codec can change, split so the largest share is named.
    for keyspace in Keyspace::ALL {
        let pairs = backend.scan(&ScanRequest::new(*keyspace, KeyRange::all()))?;
        let keys: usize = pairs.iter().map(|(key, _)| key.as_slice().len()).sum();
        let values: usize = pairs.iter().map(|(_, value)| value.as_slice().len()).sum();
        println!(
            "series_bytes: region={keyspace:?} entries={} key_bytes={keys} value_bytes={values}",
            pairs.len()
        );
        if std::env::var_os("SERIES_BYTES_DUMP").is_some()
            && let Some((key, value)) = pairs.get(pairs.len() / 2)
        {
            println!("  key   {:02x?}", key.as_slice());
            println!(
                "  value {:02x?}",
                &value.as_slice()[..value.as_slice().len().min(160)]
            );
        }
    }
    drop(backend);
    let bytes = sst_bytes(dir)?;
    // Hundredths of a byte, in integers, so no conversion can lose precision.
    let hundredths = bytes
        .checked_mul(100)
        .and_then(|scaled| scaled.checked_div(points))
        .ok_or("no points written")?;
    println!(
        "series_bytes: points={points} time={time} sst_bytes={bytes} bytes_per_point={}.{:02}",
        hundredths / 100,
        hundredths % 100
    );
    Ok(())
}
