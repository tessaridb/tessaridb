//! What several readers reading at once cost, against one reader.
//!
//! # The question
//!
//! Readers take no write turn, so nothing in the store serialises them on
//! purpose — but every point read goes through the engine's block cache, and a
//! cache is shared state. The numbers worth having are how reads per second move
//! as readers are added and what the tail does, which is where contention inside
//! the read path shows and nowhere else (G040 SG5: which block cache kind).
//!
//! # How it is measured
//!
//! One table of [`RECORDS`] records, written once in transactions of [`SEED_BATCH`]
//! so that seeding a disk store costs a few syncs rather than one per record. Each
//! reader then reads [`PER_READER`] records by identity, spread over the whole
//! table and offset per reader. Throughput is the wall clock of the phase divided
//! into the reads, as in `concurrent`.

use std::fmt::Write as _;
use std::sync::Barrier;
use std::time::{Duration, Instant};

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// Records in the table the readers read.
const RECORDS: u64 = 20_000;

/// Records written per seeding transaction.
const SEED_BATCH: u64 = 1_000;

/// Reads each reader makes per phase.
const PER_READER: u64 = 20_000;

/// The reader counts to compare, one phase each.
const READERS: [u64; 5] = [1, 2, 4, 8, 16];

/// Point reads from several threads at once.
pub(crate) fn readers(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION shelf;")?;
    let mut first = 0_u64;
    while first < RECORDS {
        let mut script = String::from("BEGIN;");
        for id in first..first.saturating_add(SEED_BATCH).min(RECORDS) {
            write!(
                script,
                " CREATE shelf:{id} = {{ n: {id}, label: 'item {id}' }};"
            )?;
        }
        script.push_str(" COMMIT;");
        session.run(&script)?;
        first = first.saturating_add(SEED_BATCH);
    }

    let mut reports = Vec::new();
    for readers in READERS {
        let (samples, wall) = round(db, readers)?;
        let reads = readers.saturating_mul(PER_READER);
        reports.push(samples.summarise(&format!("readers-{readers}")));
        reports.push(Report::measurement(
            &format!("readers-{readers}-wall"),
            &format!(
                "{reads} reads, {:.0} reads/s over {:.1} ms",
                per_second(reads, wall),
                wall.as_secs_f64() * 1_000.0
            ),
        ));
    }
    Ok(reports)
}

/// One phase: `readers` threads, each reading its share.
fn round(db: &Db, readers: u64) -> Failable<(Samples, Duration)> {
    let start = Barrier::new(usize::try_from(readers)?.saturating_add(1));
    let (finished, wall) = std::thread::scope(|scope| {
        let running: Vec<_> = (0..readers)
            .map(|reader| {
                let start = &start;
                scope.spawn(move || one_reader(db, reader, start))
            })
            .collect();
        start.wait();
        let began = Instant::now();
        let finished: Vec<_> = running.into_iter().map(|handle| handle.join()).collect();
        (finished, began.elapsed())
    });
    let mut samples = Samples::with_capacity(usize::try_from(readers.saturating_mul(PER_READER))?);
    for joined in finished {
        for sample in joined.map_err(|_| "a reader thread panicked")?? {
            samples.push(sample);
        }
    }
    Ok((samples, wall))
}

/// One reader's latencies. A read that fails or finds nothing fails the phase:
/// a fast miss would describe a store that answered nothing as a fast one.
fn one_reader(db: &Db, reader: u64, start: &Barrier) -> Result<Vec<Duration>, String> {
    let mut session = db.session();
    session
        .run("USE NAMESPACE bench; USE DATABASE bench;")
        .map_err(|failure| failure.to_string())?;
    let mut taken = Vec::with_capacity(usize::try_from(PER_READER).unwrap_or(0));
    start.wait();
    for n in 0..PER_READER {
        // A stride coprime to the table size walks all of it, offset per reader.
        let id = n
            .saturating_mul(7_919)
            .saturating_add(reader.saturating_mul(101))
            % RECORDS;
        let started = Instant::now();
        let outcomes = session
            .run(&format!("SELECT * FROM shelf:{id};"))
            .map_err(|failure| failure.to_string())?;
        taken.push(started.elapsed());
        let found = outcomes
            .last()
            .and_then(|outcome| outcome.records())
            .is_some_and(|records| !records.is_empty());
        if !found {
            return Err(format!("shelf:{id} was not found"));
        }
    }
    Ok(taken)
}

/// Reads per second, or zero for an empty phase.
fn per_second(reads: u64, wall: Duration) -> f64 {
    let seconds = wall.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    f64::from(u32::try_from(reads).unwrap_or(u32::MAX)) / seconds
}
