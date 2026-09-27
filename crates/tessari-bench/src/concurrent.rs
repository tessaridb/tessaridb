//! What several writers committing at once cost, against one writer.
//!
//! # The question
//!
//! Every commit into one log allocates the next position of that log, and a
//! position is taken by reading the committed tail and applying the batch on the
//! condition that the tail has not moved. Writers that read the same tail race;
//! the losers wait and try again, up to a fixed number of attempts, and a writer
//! that loses every attempt is refused. On disk, each winning batch is also one
//! device sync.
//!
//! So the numbers worth having are the ones a single-writer run cannot show:
//! how throughput moves as writers are added, what the tail latency does, and
//! how many commits are **refused** rather than slowed. A refusal is reported as
//! its own count because a run that only timed the successes would describe a
//! store that was quietly dropping work as a fast one.
//!
//! # How it is measured
//!
//! Every writer inserts distinct records into one table, so no two writes touch
//! the same record and every refusal is a lost race for the position rather than
//! a genuine conflict. Throughput is the wall clock from the first write to the
//! last, divided into the writes that landed — summed per-operation time would
//! count the same second once per thread.

use std::sync::Barrier;
use std::time::{Duration, Instant};

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How many records each writer inserts.
const PER_WRITER: u64 = 200;

/// The writer counts to compare, one phase each.
const WRITERS: [u64; 5] = [1, 2, 4, 8, 16];

/// Point writes from several threads at once into one table.
pub(crate) fn concurrent(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    db.session()
        .run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION crowd;")?;

    let mut reports = Vec::new();
    for writers in WRITERS {
        let (samples, refused, wall, why) = round(db, writers)?;
        let landed = writers.saturating_mul(PER_WRITER).saturating_sub(refused);
        reports.push(samples.summarise(&format!("writers-{writers}")));
        reports.push(Report::measurement(
            &format!("writers-{writers}-wall"),
            &format!(
                "{landed} landed, {refused} refused, {:.0} commits/s over {:.1} ms{}",
                per_second(landed, wall),
                wall.as_secs_f64() * 1_000.0,
                why.map(|first| format!(" — first refusal: {first}"))
                    .unwrap_or_default()
            ),
        ));
    }
    Ok(reports)
}

/// One phase: `writers` threads, each inserting its own records.
///
/// Returns every landed write's latency, how many were refused, the wall clock
/// of the whole phase, and what the first refusal said — a count of refusals
/// with no reason cannot tell lost races from a harness that wrote nonsense.
fn round(db: &Db, writers: u64) -> Failable<(Samples, u64, Duration, Option<String>)> {
    let start = Barrier::new(usize::try_from(writers)?.saturating_add(1));
    let outcomes = std::thread::scope(|scope| {
        let running: Vec<_> = (0..writers)
            .map(|writer| {
                let start = &start;
                scope.spawn(move || one_writer(db, writers, writer, start))
            })
            .collect();
        // Released together, and the clock starts when they are.
        start.wait();
        let began = Instant::now();
        let finished: Vec<_> = running.into_iter().map(|handle| handle.join()).collect();
        (finished, began.elapsed())
    });

    let (finished, wall) = outcomes;
    let mut samples = Samples::with_capacity(usize::try_from(writers.saturating_mul(PER_WRITER))?);
    let mut refused = 0_u64;
    let mut why = None;
    for joined in finished {
        let (taken, lost, first) = joined.map_err(|_| "a writer thread panicked")??;
        for sample in taken {
            samples.push(sample);
        }
        refused = refused.saturating_add(lost);
        why = why.or(first);
    }
    Ok((samples, refused, wall, why))
}

/// What one writer measured: its latencies, how many it lost, and why the first
/// one was lost.
type Share = (Vec<Duration>, u64, Option<String>);

/// One writer's share of a phase.
fn one_writer(db: &Db, writers: u64, writer: u64, start: &Barrier) -> Result<Share, String> {
    let mut session = db.session();
    session
        .run("USE NAMESPACE bench; USE DATABASE bench;")
        .map_err(|failure| failure.to_string())?;
    let mut taken = Vec::with_capacity(usize::try_from(PER_WRITER).unwrap_or(0));
    let mut lost = 0_u64;
    let mut why = None;
    start.wait();
    // Distinct across every phase and writer, so no two writes ever name the
    // same record: phase in the millions, writer in the thousands.
    let first = writers
        .saturating_mul(1_000_000)
        .saturating_add(writer.saturating_mul(1_000));
    for n in 0..PER_WRITER {
        let id = first.saturating_add(n);
        let started = Instant::now();
        let written = session.run(&format!(
            "CREATE crowd:{id} = {{ writer: {writer}, n: {n} }};"
        ));
        match written {
            Ok(_) => taken.push(started.elapsed()),
            Err(failure) => {
                lost = lost.saturating_add(1);
                why = why.or_else(|| Some(failure.to_string()));
            }
        }
    }
    Ok((taken, lost, why))
}

/// Commits per second, or zero for an empty phase.
fn per_second(landed: u64, wall: Duration) -> f64 {
    let seconds = wall.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    // Exact below 2^53, which a benchmark phase does not approach.
    f64::from(u32::try_from(landed).unwrap_or(u32::MAX)) / seconds
}
