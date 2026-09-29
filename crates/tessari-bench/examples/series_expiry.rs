//! What removing a series' aged points costs (G044 C11).
//!
//! `cargo run --release -p tessari-bench --example series_expiry -- <dir> [aged] [kept]`
//!
//! Writes `aged` points named in 2025 and `kept` points named a minute ago to a
//! series retaining 30 days, in a fresh store at `<dir>`. Then it times one call
//! of the removal pass, checks the answer is the same record for record before
//! and after, counts the log's entries, and reports the SST bytes
//! left after a forced compaction. The same file runs against two builds, so the
//! instrument is the same and only the pass differs.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tessari_kv::{KeyRange, Keyspace, KvBackend, ScanRequest};
use tessari_lsm::{LsmBackend, StoreConfig};
use tessari_storage::Catalog;
use tessaridb::Db;

const BATCH: u64 = 10_000;

/// 2025-01-01T00:00:00Z in milliseconds: every aged point is older than 30 days.
const AGED_FROM_MS: u64 = 1_735_689_600_000;

/// A UUID version 7 literal naming `at_ms`, `counter` in its random bits.
fn named(at_ms: u64, counter: u64) -> String {
    let stamp = format!("{:012x}", at_ms & 0x0000_ffff_ffff_ffff);
    let (high, low) = stamp.split_at(8);
    format!(
        "{high}-{low}-7{:03x}-8{:03x}-{:012x}",
        (counter >> 12) & 0xfff,
        counter & 0xfff,
        counter & 0x0000_ffff_ffff_ffff,
    )
}

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

/// Write `count` points from `from_ms`, one a second, in batches.
fn write(
    session: &mut tessaridb::Session,
    from_ms: u64,
    first: u64,
    count: u64,
) -> Result<(), Box<dyn Error>> {
    let mut written = 0_u64;
    while written < count {
        let mut script = String::from("BEGIN;");
        let upto = written.saturating_add(BATCH).min(count);
        for n in written..upto {
            let counter = first.saturating_add(n);
            let at = from_ms.saturating_add(n.saturating_mul(1_000));
            script.push_str(&format!(
                " CREATE readings:uuid '{}' = {{ sensor: 's{}', v: {} }};",
                named(at, counter),
                n % 10,
                n % 997
            ));
        }
        script.push_str(" COMMIT;");
        session.run(&script)?;
        written = upto;
    }
    Ok(())
}

/// The identities the series answers with, in order.
fn answer(session: &mut tessaridb::Session) -> Result<Vec<String>, Box<dyn Error>> {
    let outcomes = session.run("SELECT * FROM readings;")?;
    Ok(outcomes
        .first()
        .and_then(tessaridb::Outcome::records)
        .map(|records| records.iter().map(|(id, _)| id.to_literal()).collect())
        .unwrap_or_default())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    let dir = Path::new(args.get(1).ok_or("a store directory")?);
    let aged: u64 = args.get(2).map_or(Ok(1_000_000), |held| held.parse())?;
    let kept: u64 = args.get(3).map_or(Ok(10_000), |held| held.parse())?;

    let db = Db::open(dir)?;
    let mut session = db.session();
    session.run(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench; DEFINE DATABASE bench; \
         USE DATABASE bench; DEFINE SERIES readings RETAIN 30d;",
    )?;
    let now_ms = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    write(&mut session, AGED_FROM_MS, 0, aged)?;
    // A minute ago, a millisecond apart, so none of them nears the floor.
    write(&mut session, now_ms.saturating_sub(60_000), aged, kept)?;

    let before = answer(&mut session)?;
    let store = db.store();
    let (namespace, database, table) = {
        let mut transaction = store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("bench")?.ok_or("no namespace")?;
        let database = catalog
            .database_id(namespace, "bench")?
            .ok_or("no database")?;
        let table = catalog
            .table_id(namespace, database, "readings")?
            .ok_or("no table")?;
        (namespace, database, table)
    };
    let started = Instant::now();
    let expired = store.expire_series(namespace, database, table)?;
    let took = started.elapsed();
    let after = answer(&mut session)?;
    let same = before == after && u64::try_from(after.len())? == kept;
    drop(session);
    drop(db);

    let backend = LsmBackend::open(dir, StoreConfig::default())?;
    backend.compact()?;
    let count = |keyspace| -> Result<usize, Box<dyn Error>> {
        Ok(backend
            .scan(&ScanRequest::new(keyspace, KeyRange::all()))?
            .len())
    };
    // The writes are a known number of commits, so what the pass added to the
    // log is what this exceeds that number by.
    let logged = count(Keyspace::LOG)?;
    let records = count(Keyspace::DATA)?;
    drop(backend);
    println!(
        "series_expiry: aged={aged} kept={kept} ranges={} indexed={} millis={} log_entries={logged} \
         answer_unchanged={same} record_entries_after={records} sst_bytes={}",
        expired.ranges,
        expired.indexed,
        took.as_millis(),
        sst_bytes(dir)?
    );
    Ok(())
}
