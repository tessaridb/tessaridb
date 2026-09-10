//! What a window over identity costs, asked three ways.
//!
//! # The claim this exists to confirm or refute
//!
//! A span of identities is meant to be the cheap way to read a window, because
//! it consults nothing: a record's key is its table prefix followed by its
//! identity, so the records outside the span are never read, never decoded and
//! never tested. The alternatives are a table scan testing every record, and a
//! value index over a field that mirrors the identity — which on monotonic data
//! is a second copy of an ordering the primary key already has.
//!
//! The claim is therefore two claims and they need different evidence:
//!
//! - the three reads answer the **same records**, which is correctness and is
//!   the half a timing table cannot speak to;
//! - the span is the **fastest**, which is the reason it exists.
//!
//! # Why the span runs first
//!
//! The order is deliberate and it is the opposite of the flattering one. The
//! span touches two per cent of the table and runs against the coldest cache
//! there is; the scan that follows reads everything and warms it; the index runs
//! last, on a table two passes have already warmed. So the arrangement is
//! stacked *against* the reading this workload is shaped to produce, and a span
//! that wins from that position wins for a reason other than its turn.
//!
//! # What it does not measure
//!
//! Whether the index is worth having at all. Its build is timed and reported,
//! and the answer to that question is a different one — an index serves reads
//! this workload does not ask, and paying for it once is not the same trade as
//! paying for it per read.

use std::time::Instant;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How many records the table holds.
///
/// Above `PLANNER_SCAN_FLOOR_RECORDS`, below which the planner does not weigh an
/// index against the table at all — a smaller table would measure the guard's
/// absence rather than the paths.
const RECORDS: u64 = 50_000;

/// How wide one window is: two per cent of the table.
///
/// Narrow enough that an index is genuinely worth serving, which is the case the
/// span has to beat. A window that returned most of the table would be answered
/// by the scan whatever the plan said, and would compare nothing.
const WINDOW: u64 = 1_000;

/// How many windows are read per phase.
const QUERIES: u64 = 100;

/// Where one window begins, so the three phases ask for the same records.
fn lower(query: u64) -> u64 {
    (query % 40).saturating_mul(WINDOW)
}

/// The identities one answer names.
fn answered(outcomes: &[tessaridb::Outcome]) -> Vec<String> {
    outcomes
        .first()
        .and_then(tessaridb::Outcome::records)
        .map(|records| records.iter().map(|(id, _)| id.to_literal()).collect())
        .unwrap_or_default()
}

/// The same window, three ways.
///
/// # Errors
///
/// Returns whatever the store refuses.
pub(crate) fn span(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE TABLE windows SCHEMALESS;")?;

    // The identity and the field carry the same number, which is what makes the
    // three reads comparable: the span walks the key, the other two test the
    // field, and they are asking one question.
    for n in 0..RECORDS {
        session.run(&format!(
            "CREATE windows:{n} = {{ n: {n}, note: 'a payload long enough to be a record rather than a token' }};"
        ))?;
    }

    let mut reports = Vec::new();

    // Coldest first. See the module header: this order is against the result.
    let mut ranged = Samples::with_capacity(usize::try_from(QUERIES).unwrap_or(0));
    for query in 0..QUERIES {
        let from = lower(query);
        let upto = from.saturating_add(WINDOW);
        let started = Instant::now();
        session.run(&format!("SELECT * FROM windows:{from}..{upto};"))?;
        ranged.push(started.elapsed());
    }
    reports.push(ranged.summarise("window by span"));

    let mut scanned = Samples::with_capacity(usize::try_from(QUERIES).unwrap_or(0));
    for query in 0..QUERIES {
        let from = lower(query);
        let upto = from.saturating_add(WINDOW);
        let started = Instant::now();
        session.run(&format!(
            "SELECT * FROM windows WHERE n >= {from} AND n < {upto};"
        ))?;
        scanned.push(started.elapsed());
    }
    reports.push(scanned.summarise("window by scan"));

    let mut built = Samples::with_capacity(1);
    let started = Instant::now();
    session.run("DEFINE INDEX by_n ON windows FIELDS n;")?;
    built.push(started.elapsed());
    reports.push(built.summarise("the index, built once"));

    let mut served = Samples::with_capacity(usize::try_from(QUERIES).unwrap_or(0));
    for query in 0..QUERIES {
        let from = lower(query);
        let upto = from.saturating_add(WINDOW);
        let started = Instant::now();
        session.run(&format!(
            "SELECT * FROM windows WHERE n >= {from} AND n < {upto};"
        ))?;
        served.push(started.elapsed());
    }
    reports.push(served.summarise("window by value index"));

    // The half a timing table cannot speak to. One window, all three ways, with
    // the answers compared record by record rather than by count — two reads
    // over the same window can return the same number of different records, and
    // a count would report that as agreement.
    let from = lower(1);
    let upto = from.saturating_add(WINDOW);
    let by_span = answered(&session.run(&format!("SELECT * FROM windows:{from}..{upto};"))?);
    let by_field = answered(&session.run(&format!(
        "SELECT * FROM windows WHERE n >= {from} AND n < {upto};"
    ))?);
    session.run("DROP INDEX by_n ON windows;")?;
    let by_scan = answered(&session.run(&format!(
        "SELECT * FROM windows WHERE n >= {from} AND n < {upto};"
    ))?);

    reports.push(Report::measurement(
        "  records answered",
        &format!(
            "span {} | index {} | scan {}",
            by_span.len(),
            by_field.len(),
            by_scan.len()
        ),
    ));
    reports.push(Report::measurement(
        "  the three record sets",
        if by_span == by_field && by_field == by_scan {
            "identical, compared record by record"
        } else {
            "DIFFER — the three reads do not answer the same records"
        },
    ));
    Ok(reports)
}
