//! What the planner's scan guard is worth, measured rather than reasoned about.
//!
//! # The claim this workload exists to settle
//!
//! An ordered index that selects most of a table was chosen unconditionally,
//! because the scan was not a candidate and had no number to be compared
//! against. That cost was measured once, on 2026-09-07, and the store then
//! gained `plan::worth_serving`: an index is served only when it can produce at
//! most half the table. The path change was proven by thirteen cases. The
//! **speed** change was not proven by anything, and a guard whose benefit was
//! never measured is a belief with a test suite behind it.
//!
//! # Why this does not compare against the old number
//!
//! That figure was taken on a fixture nobody wrote down, so a ratio against it
//! would have a denominator that cannot be reconstructed. Instead the planner
//! this store had *before* the guard is reproduced in the same process by
//! `WITHOUT SCAN GUARD`, which returns from `worth_serving` before anything is
//! counted and therefore restores exactly the behaviour that shipped earlier.
//! Two arms in one run, on one table, are internally valid; two numbers from
//! different weeks are not.
//!
//! # Why the two arms share a table and alternate
//!
//! The guarded read and the lifted read differ by one clause and by nothing
//! else — same records, same pages, whatever the cache happens to hold. They
//! are issued alternately inside one loop so that anything drifting during the
//! run drifts through both of them equally. A pair measured in two consecutive
//! blocks would attribute that drift to the clause.
//!
//! The unindexed mirror is a **fixture control** and not the control: it says
//! whether the guarded table under veto costs what a table with no index at all
//! costs. If it does not, the fixture is wrong and the other two numbers mean
//! less than they appear to.
//!
//! # The half a timing table cannot speak to
//!
//! A faster answer that is a different answer is not a faster answer. The
//! guarded and lifted reads are compared record by record, which is available
//! to them because they read the same table. The mirror is compared by count
//! alone and this is stated rather than glossed: its records are named after a
//! different table, so their identities differ by construction and an identity
//! comparison against them would be reporting the table's name.

use core::time::Duration;
use std::time::Instant;

use tessaridb::{Db, Outcome};

use crate::ranges::NOTE;
use crate::samples::{Report, Samples};
use crate::span::answered;
use crate::workload::{Failable, prepared};

/// How many records each table holds.
///
/// Far above `PLANNER_SCAN_FLOOR_RECORDS`, below which the guard does not weigh
/// an index against the table at all — a smaller table would measure the
/// guard's absence. The same size the other planner-shaped workloads here use,
/// so their figures sit beside each other.
const RECORDS: u64 = 50_000;

/// How many times each arm reads.
///
/// Every read returns the whole table, so twenty is already a million records
/// answered per arm. A larger count would buy precision the ratio does not
/// need and would make the run's wall time the reason nobody re-runs it.
const QUERIES: usize = 20;

/// The guarded read: an index is declared over `n` and the veto decides.
const GUARDED: &str = "SELECT * FROM guarded WHERE n > 0;";

/// The same read with the veto lifted — the planner as it was before the guard.
const LIFTED: &str = "SELECT * FROM guarded WHERE n > 0 WITHOUT SCAN GUARD;";

/// The same predicate over a table where no index was ever declared.
const UNINDEXED: &str = "SELECT * FROM mirror WHERE n > 0;";

/// The guarded table's own scan, read before its index exists.
///
/// Textually identical to [`GUARDED`] and named separately because the two are
/// different measurements: this one runs while nothing has been indexed, and the
/// only event between it and its twin is the index build.
const BEFORE_INDEX: &str = "SELECT * FROM guarded WHERE n > 0;";

/// What the scan guard is worth on a read that selects the whole table.
///
/// # Errors
///
/// Returns whatever the store refuses.
pub(crate) fn guard(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE TABLE guarded SCHEMALESS;\n\
         DEFINE TABLE mirror SCHEMALESS;",
    )?;

    // Both tables are filled in one loop so neither is written into a warmer
    // store than the other. Only the guarded write is timed: what a write costs
    // is not what this workload is asking.
    let mut written = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        let started = Instant::now();
        session.run(&format!(
            "CREATE guarded:{n} = {{ n: {n}, note: '{NOTE}' }};"
        ))?;
        written.push(started.elapsed());
        session.run(&format!(
            "CREATE mirror:{n} = {{ n: {n}, note: '{NOTE}' }};"
        ))?;
    }
    let mut reports = vec![written.summarise("guard-write")];

    // The same table's scan while no index exists anywhere in the store. This is
    // the one comparison that cannot be confounded by layout: it is one table
    // against itself, and the index build is the only event between the two
    // readings. A drop-and-re-measure would leave tombstones behind and answer
    // ambiguously in exactly the direction the question cares about.
    session.run(BEFORE_INDEX)?;
    let mut before = Samples::with_capacity(QUERIES);
    let mut before_path = None;
    for _ in 0..QUERIES {
        let started = Instant::now();
        let outcome = session.run(BEFORE_INDEX)?;
        before.push(started.elapsed());
        before_path = served_by(&outcome);
    }
    let unindexed_answer = answered(&session.run(BEFORE_INDEX)?);

    let mut built = Samples::with_capacity(1);
    let started = Instant::now();
    session.run("DEFINE INDEX by_n ON guarded FIELDS n;")?;
    built.push(started.elapsed());
    reports.push(built.summarise("guard-build-index"));

    // One untimed read of each shape, so no arm pays for being the one that
    // arrived first at a cold page.
    session.run(GUARDED)?;
    session.run(LIFTED)?;
    session.run(UNINDEXED)?;

    let mut guarded = Samples::with_capacity(QUERIES);
    let mut lifted = Samples::with_capacity(QUERIES);
    let mut guarded_path = None;
    let mut lifted_path = None;
    for _ in 0..QUERIES {
        let started = Instant::now();
        let outcome = session.run(GUARDED)?;
        guarded.push(started.elapsed());
        guarded_path = served_by(&outcome);

        let started = Instant::now();
        let outcome = session.run(LIFTED)?;
        lifted.push(started.elapsed());
        lifted_path = served_by(&outcome);
    }

    let mut mirrored = Samples::with_capacity(QUERIES);
    let mut mirrored_path = None;
    for _ in 0..QUERIES {
        let started = Instant::now();
        let outcome = session.run(UNINDEXED)?;
        mirrored.push(started.elapsed());
        mirrored_path = served_by(&outcome);
    }

    let untouched = before.summarise("guard-before-any-index");
    let under_guard = guarded.summarise("guard-veto-raised");
    let without_guard = lifted.summarise("guard-lifted");
    let no_index = mirrored.summarise("guard-mirror-unindexed");

    // The path each arm actually took, read out of the read itself rather than
    // asked for separately, so the row and the timing beside it cannot disagree
    // about which plan produced the number.
    reports.push(Report::measurement(
        "  served by",
        &format!(
            "before any index: {} | veto raised: {} | lifted: {} | mirror: {}",
            before_path.unwrap_or("nothing"),
            guarded_path.unwrap_or("nothing"),
            lifted_path.unwrap_or("nothing"),
            mirrored_path.unwrap_or("nothing")
        ),
    ));
    reports.push(Report::measurement(
        "  lifted / veto raised",
        &format!(
            "{} — the penalty the guard declines, one table, one clause apart",
            ratio(without_guard.p50, under_guard.p50)
        ),
    ));
    reports.push(Report::measurement(
        "  lifted / mirror",
        &format!(
            "{} — the shape the original measurement was taken in",
            ratio(without_guard.p50, no_index.p50)
        ),
    ));
    reports.push(Report::measurement(
        "  veto raised / before any index",
        &format!(
            "{} — one table against itself; what an index the planner REFUSES still costs",
            ratio(under_guard.p50, untouched.p50)
        ),
    ));
    reports.push(Report::measurement(
        "  before any index / mirror",
        &format!(
            "{} — the two tables before either carries an index; far from 1 is a fixture defect",
            ratio(untouched.p50, no_index.p50)
        ),
    ));
    reports.push(Report::measurement(
        "  veto raised / mirror",
        &format!(
            "{} — the fixture control; far from 1 means the two tables are not comparable",
            ratio(under_guard.p50, no_index.p50)
        ),
    ));

    let by_veto = answered(&session.run(GUARDED)?);
    let by_index = answered(&session.run(LIFTED)?);
    let by_mirror = answered(&session.run(UNINDEXED)?);
    reports.push(Report::measurement(
        "  records answered",
        &format!(
            "veto raised {} | lifted {} | mirror {}",
            by_veto.len(),
            by_index.len(),
            by_mirror.len()
        ),
    ));
    reports.push(Report::measurement(
        "  the two paths over one table",
        if by_veto == by_index {
            "identical, compared record by record"
        } else {
            "DIFFER — the guard changed which records a read answers"
        },
    ));
    reports.push(Report::measurement(
        "  the one table across its index build",
        if unindexed_answer == by_veto {
            "identical, compared record by record"
        } else {
            "DIFFER — declaring an index changed what a scan of the table answers"
        },
    ));
    reports.push(Report::measurement(
        "  the mirror",
        if by_mirror.len() == by_veto.len() {
            "same count; identities name a different table, so they are not compared"
        } else {
            "DIFFERENT COUNT — the fixture is not the same data"
        },
    ));

    reports.push(untouched);
    reports.push(under_guard);
    reports.push(without_guard);
    reports.push(no_index);
    Ok(reports)
}

/// The access path a read reports for itself.
fn served_by(outcomes: &[Outcome]) -> Option<&'static str> {
    match outcomes.last() {
        Some(Outcome::Records { plan, .. }) => Some(plan.access.name()),
        _ => None,
    }
}

/// `over / under`, to two places, in integer arithmetic.
///
/// A ratio is the whole point of this workload, so it is computed here rather
/// than left for a reader to divide two percentiles by eye and get wrong.
fn ratio(over: Duration, under: Duration) -> String {
    // Checked throughout rather than guarded once: a denominator that rounds to
    // zero is a real outcome here, because a phase this workload times could in
    // principle come back below the clock's resolution, and a ratio is exactly
    // the place where that turns into a panic in a release binary.
    let Some(scaled) = over
        .as_nanos()
        .saturating_mul(100)
        .checked_div(under.as_nanos())
    else {
        return "not computable — the denominator was below the clock".to_owned();
    };
    let whole = scaled.checked_div(100).unwrap_or_default();
    let hundredths = scaled.checked_rem(100).unwrap_or_default();
    format!("{whole}.{hundredths:02}x")
}
