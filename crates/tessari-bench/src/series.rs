//! What the declared retention clause costs, and whether it reads the table
//! whole.
//!
//! # The claim this exists to confirm or refute
//!
//! `DEFINE SERIES … RETAIN` declares a floor, and `Store::expire_series` removes
//! what the floor has stopped answering with. The pass scans a bounded key range
//! — from the table's first key up to the floor — so by construction it should
//! never touch a record the table keeps. That is a claim about **shape**: at one
//! window width the pass should cost the same on a large table as on a small
//! one.
//!
//! The criterion it serves is G023's S4.2, *"retention exists as a declared
//! clause and drops a range without reading the table whole"*, whose validation
//! method is *"measured against `DELETE WHERE`"*.
//!
//! # Why this is not the `retention` workload again
//!
//! `retention` measured the **statement** — `DELETE FROM t:a..b` — whose cost
//! argument the specification already wrote. What is measured here is a
//! different mechanism that had never been run against a growing table: a
//! background pass over a clock-derived floor, writing its removals through the
//! ordinary write path.
//!
//! # Why the table grows and the window does not
//!
//! Because a single head-to-head at one table size cannot answer *"without
//! reading the table whole"*: whichever arm wins, both numbers are consistent
//! with both arms reading everything. Holding the removed window constant and
//! growing what the table keeps separates a flat cost from a linear one in one
//! run, and needs no profiler to read.
//!
//! # Why both arms are named record by record
//!
//! Every identity in both tables is written explicitly, the removed window at
//! instants in 2024 and the kept records at instants a few seconds old. Two
//! reasons. The floor is derived from the clock, so putting the two populations
//! two years apart means no record can drift across it while a cell builds. And
//! naming them means the two arms hold the **same** identities, so what remains
//! can be compared record by record rather than by count — which is
//! load-bearing here, because two removals can leave the same number of
//! different records and a count would report that as agreement.
//!
//! # What the number is, and what it is not
//!
//! It is the cost of the pass, commits included: a removal here is a `DELETE`
//! like any other, sequenced into the log and carried on the change feed. It is
//! therefore not a scan figure and must not be read as one. The pass commits
//! every 512 records, so a thousand-record window is two commits at every table
//! size — constant across the sizes, which is what the shape question needs.
//!
//! # What it does not measure
//!
//! When the pass should run. Nothing schedules it, deliberately, and the
//! interval is an operational decision that wants a measurement of its own.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tessari_storage::Catalog;
use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How many records each table keeps, spanning eight times from end to end.
///
/// Eight times is chosen so that a cost proportional to what the table keeps
/// cannot be mistaken for noise: it has to move by most of that factor, and a
/// flat arm has to stay flat across it.
const SIZES: &[u64] = &[5_000, 10_000, 20_000, 40_000];

/// How many records each run removes, at every size.
///
/// Constant on purpose — it is the independent variable held still.
const WINDOW: u64 = 1_000;

/// How many times each cell is built and measured.
const REPEATS: usize = 3;

/// How far back the declared floor reaches.
///
/// An hour, against a removed window named in 2024 and kept records named
/// seconds ago. The margin either side is enormous compared with how long a cell
/// takes to build, so nothing crosses the floor mid-measurement.
const RETAIN: &str = "1h";

/// Milliseconds since the epoch at 2024-01-01T00:00:00Z.
///
/// The removed window is spread forward from here, one millisecond apart, so it
/// reads as a series rather than as a thousand records at one instant.
const ANCIENT_MS: u64 = 1_704_067_200_000;

/// A UUID v7 literal naming `at_ms`, with `counter` in its random bits.
///
/// Version 7 carries the millisecond in its leading six bytes, big-endian, which
/// is what makes an age floor a position in the key rather than a predicate over
/// a field. The version nibble is `7` and the variant nibble is `8`; everything
/// else is filled from the counter so that identities at the same instant are
/// still distinct.
fn named(at_ms: u64, counter: u64) -> String {
    let stamp = format!("{:012x}", at_ms & 0x0000_ffff_ffff_ffff);
    let (high, low) = stamp.split_at(8);
    let tail = counter & 0x0000_ffff_ffff_ffff;
    format!(
        "{high}-{low}-7{:03x}-8{:03x}-{tail:012x}",
        (counter >> 12) & 0xfff,
        counter & 0xfff,
    )
}

/// Now, in milliseconds since the epoch.
///
/// # Errors
///
/// Returns an error when the clock is before the epoch, which is not a case this
/// harness invents a value for.
fn now_ms() -> Failable<u64> {
    let since = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(u64::try_from(since.as_millis())?)
}

/// Fill `table` with `WINDOW` records below the floor and `keeps` above it.
///
/// Both arms are built by this one function, so the two tables differ in nothing
/// but the kind they were declared as.
fn build(
    session: &mut tessaridb::Session,
    declaration: &str,
    table: &str,
    keeps: u64,
    recent_ms: u64,
) -> Failable<()> {
    session.run(declaration)?;
    for n in 0..WINDOW {
        let id = named(ANCIENT_MS.saturating_add(n), n);
        session.run(&format!(
            "CREATE {table}:uuid '{id}' = {{ n: {n}, note: 'a payload long enough to be a record rather than a token' }};"
        ))?;
    }
    for n in 0..keeps {
        let id = named(recent_ms, WINDOW.saturating_add(n));
        session.run(&format!(
            "CREATE {table}:uuid '{id}' = {{ n: {}, note: 'a payload long enough to be a record rather than a token' }};",
            WINDOW.saturating_add(n)
        ))?;
    }
    Ok(())
}

/// The identities a table still answers with, in the order it answers.
fn remaining(session: &mut tessaridb::Session, table: &str) -> Failable<Vec<String>> {
    let outcomes = session.run(&format!("SELECT * FROM {table};"))?;
    Ok(outcomes
        .first()
        .and_then(tessaridb::Outcome::records)
        .map(|records| records.iter().map(|(id, _)| id.to_literal()).collect())
        .unwrap_or_default())
}

/// Run the expiry pass over one table, resolved by name.
///
/// Nothing in the language runs this pass, so the harness reaches the store the
/// way the backup tool does. That is a property of the harness and not of the
/// engine: a caller with only TessariQL has no way to ask for it today.
fn expire(db: &Db, table: &str) -> Failable<usize> {
    let store = db.store();
    let (namespace, database, id) = {
        let mut transaction = store.begin()?;
        let catalog = Catalog::new(&mut transaction);
        let namespace = catalog
            .namespace_id("bench")?
            .ok_or("the bench namespace is missing")?;
        let database = catalog
            .database_id(namespace, "bench")?
            .ok_or("the bench database is missing")?;
        let id = catalog
            .table_id(namespace, database, table)?
            .ok_or("the table this cell just built is missing")?;
        (namespace, database, id)
    };
    Ok(store.expire_series(namespace, database, id)?.records)
}

/// The same window removed by a declared clause and by a condition, at four
/// table sizes.
///
/// # Errors
///
/// Returns whatever the store refuses, and refuses itself if the clock is before
/// the epoch.
pub(crate) fn series(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;
    let recent_ms = now_ms()?;

    let mut reports = Vec::new();

    // Coldest first, and it is the arm expected to win — the same order the
    // `retention` and `span` workloads use, and for the same reason: a clause
    // that stays flat from the coldest position stays flat for a reason other
    // than its turn.
    for &size in SIZES {
        let mut samples = Samples::with_capacity(REPEATS);
        for round in 0..REPEATS {
            let table = format!("clause_{size}_{round}");
            build(
                &mut session,
                &format!("DEFINE SERIES {table} RETAIN {RETAIN};"),
                &table,
                size,
                recent_ms,
            )?;
            let started = Instant::now();
            expire(db, &table)?;
            samples.push(started.elapsed());
        }
        reports.push(samples.summarise(&format!("declared clause, table keeping {size}")));
    }

    for &size in SIZES {
        let mut samples = Samples::with_capacity(REPEATS);
        for round in 0..REPEATS {
            let table = format!("cond_{size}_{round}");
            build(
                &mut session,
                &format!("DEFINE TABLE {table} SCHEMALESS;"),
                &table,
                size,
                recent_ms,
            )?;
            let started = Instant::now();
            session.run(&format!(
                "DELETE FROM {table} WHERE n < {WINDOW} LIMIT ALL;"
            ))?;
            samples.push(started.elapsed());
        }
        reports.push(samples.summarise(&format!("conditional delete, table keeping {size}")));
    }

    // The half a timing table cannot speak to. Both arms named the same
    // identities, so this compares what remains record by record.
    let largest = SIZES.last().copied().unwrap_or(0);
    let after_clause = remaining(&mut session, &format!("clause_{largest}_0"))?;
    let after_cond = remaining(&mut session, &format!("cond_{largest}_0"))?;

    reports.push(Report::measurement(
        "  records remaining",
        &format!(
            "clause {} | conditional {} | expected {largest}",
            after_clause.len(),
            after_cond.len()
        ),
    ));
    reports.push(Report::measurement(
        "  the two remainders",
        if after_clause == after_cond {
            "identical, compared record by record"
        } else {
            "DIFFER — the two removals did not leave the same records"
        },
    ));
    Ok(reports)
}
