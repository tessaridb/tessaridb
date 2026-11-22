//! Row-guard overhead: what enforcing a guard costs per record, and what an
//! unguarded table pays now that the feature exists (G094 C3/C5).
//!
//! # The question
//!
//! A guard runs a compiled predicate over every delivered record of a guarded
//! table. The numbers worth having are the guard's marginal per-record cost
//! against the same read with no guard (C3), and the cost an ordinary table
//! pays for a feature it does not use — the switch-off fast path and the
//! switch-on-no-guards path both (C5).
//!
//! # How it is measured
//!
//! One table of [`RECORDS`] records, half `tenant: 'a'` and half `tenant: 'b'`,
//! seeded in transactions of [`SEED_BATCH`]. Each phase then runs [`SWEEPS`]
//! full `SELECT *` sweeps and keeps the per-record wall of the median sweep.
//! The phases differ only in who reads and whether the switch is on, so the
//! difference between the guarded phase and the unguarded ones is the guard's
//! own cost — resolution, evaluation, filtering — and not the scan's.

use std::fmt::Write as _;
use std::time::Instant;

use tessari_session::Now;
use tessari_session::RunNow;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// Records in the table the phases sweep.
const RECORDS: u64 = 100_000;

/// Records written per seeding transaction.
const SEED_BATCH: u64 = 1_000;

/// Full-table sweeps per phase; the median is the phase's number.
const SWEEPS: usize = 7;

/// Full-table sweeps as a guarded and an unguarded reader, one phase each.
pub(crate) fn guards(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    // The first user closes the store: the rest of the setup runs as the
    // owner it named, the way every store past its first statement is.
    db.session().run_now(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE COLLECTION shelf;\n\
         DEFINE USER bench_root ROLE owner PASSWORD 'bench-root';",
    )?;
    let mut root = db.session();
    root.sign_in("bench_root", "bench-root").now()?;
    root.run_now(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE USER guard_reader ON bench.bench ROLE editor PASSWORD 'bench-guard';\n\
         DEFINE USER plain_reader ON bench.bench ROLE editor PASSWORD 'bench-plain';",
    )?;
    let mut first = 0_u64;
    while first < RECORDS {
        let mut script = String::from("BEGIN;");
        for id in first..first.saturating_add(SEED_BATCH).min(RECORDS) {
            let tenant = if id % 2 == 0 { 'a' } else { 'b' };
            write!(
                script,
                " CREATE shelf:{id} = {{ n: {id}, tenant: '{tenant}' }};"
            )?;
        }
        script.push_str(" COMMIT;");
        root.run_now(&script)?;
        first = first.saturating_add(SEED_BATCH);
    }

    let mut reports = Vec::new();

    // Phase 1 — switch off: the feature exists, the table never consults it.
    // This is the C5 zero-overhead claim in its first half.
    let unguarded_ns = sweep(db, "plain_reader", "bench-plain")?;
    reports.push(phase_report("guard-off", unguarded_ns));

    // Phase 2 — switch on, a reader with no grants on the table: the flag is
    // read and the grants are asked, and nothing guards. The C5 claim in its
    // second half: enabling the switch on a table nobody guards changes
    // nothing measurable.
    root.run_now("ALTER TABLE shelf ENABLE ROW SECURITY;")?;
    let median = sweep(db, "plain_reader", "bench-plain")?;
    reports.push(phase_report("switch-on-no-guards", median));
    reports.push(delta_report("switch-on", unguarded_ns, median));

    // Phase 3 — switch on, a guarded reader: every delivered record passes
    // the compiled predicate. The delta against phase 1 is C3: the guard's
    // marginal per-record cost on this build and this host.
    root.run_now("GRANT read ON shelf WHERE (tenant = 'a') TO guard_reader;")?;
    let guarded_ns = sweep(db, "guard_reader", "bench-guard")?;
    reports.push(phase_report("guarded", guarded_ns));
    reports.push(delta_report("guard", unguarded_ns, guarded_ns));

    Ok(reports)
}

/// The median per-record wall of [`SWEEPS`] full-table sweeps as one session.
fn sweep(db: &Db, user: &str, password: &str) -> Failable<f64> {
    let mut session = db.session();
    session.sign_in(user, password).now()?;
    session.run_now("USE NAMESPACE bench; USE DATABASE bench;")?;
    let mut samples = Samples::with_capacity(SWEEPS);
    for _ in 0..SWEEPS {
        let started = Instant::now();
        session.run_now("SELECT * FROM shelf;")?;
        samples.push(started.elapsed());
    }
    let report = samples.summarise("sweep");
    // Integer nanos per record first, then one conversion — lossless for any
    // sweep under a minute, and the division never sees a float.
    let nanos_per_record = report
        .p50
        .as_nanos()
        .checked_div(u128::from(RECORDS))
        .unwrap_or(u128::MAX);
    Ok(f64::from(
        u32::try_from(nanos_per_record).unwrap_or(u32::MAX),
    ))
}

/// One phase's per-record number, as a measurement row.
fn phase_report(phase: &str, per_record_ns: f64) -> Report {
    Report::measurement(
        &format!("guards-{phase}"),
        &format!(
            "{per_record_ns:.1} ns per record, median of {SWEEPS} sweeps of {RECORDS} records"
        ),
    )
}

/// The delta between two phases, stated per record: what the second phase
/// added over the first.
fn delta_report(what: &str, baseline_ns: f64, with_ns: f64) -> Report {
    let delta = with_ns - baseline_ns;
    let share = if baseline_ns > 0.0 {
        delta / baseline_ns * 100.0
    } else {
        0.0
    };
    Report::measurement(
        &format!("guards-delta-{what}"),
        &format!("{delta:+.1} ns per record ({share:+.1} %) over the unguarded sweep"),
    )
}
