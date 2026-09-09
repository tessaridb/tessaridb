//! What a range delete costs, and whether it reads the table whole.
//!
//! # The claim this exists to confirm or refute
//!
//! The specification says of the span form of `DELETE`, at
//! `docs/tessariql.md`:
//!
//! > it costs what it removes rather than what it keeps: the conditional form
//! > reads every record it is going to keep, once per run, forever.
//!
//! That is not a claim about which of the two is faster. It is a claim that the
//! two have different **shapes as a function of table size**: at one window
//! width, the span form should cost the same on a large table as on a small
//! one, and the conditional form should cost more on the large one because
//! there is more that it keeps.
//!
//! # Why the table grows and the window does not
//!
//! Because that is what the criterion asks. *"Drops a range without reading the
//! table whole"* is a statement about what the cost depends on, and a single
//! head-to-head at one table size cannot speak to it: whichever arm wins, both
//! numbers are consistent with both arms reading everything. Holding the removed
//! window constant and growing the table separates a flat cost from a linear one
//! in one run, and needs no profiler to read.
//!
//! # Why each arm gets a freshly built table
//!
//! A delete is destructive, so the second arm cannot run on what the first left.
//! Each measurement therefore builds its own table and the build is outside the
//! timer, which also makes the two arms start from tables that are identical
//! rather than merely similar.
//!
//! # Why the span arm runs first
//!
//! The order is stacked against the expected result, as `span` does for the same
//! reason: the span arm runs on the coldest cache the process has, and the
//! conditional arm runs afterwards on a store two passes have warmed. A span
//! that stays flat from that position stays flat for a reason other than its
//! turn.
//!
//! # What three samples per cell means
//!
//! Each cell is built and measured three times, so the reported p99 is the
//! slowest of three rather than a tail. The `ops` column says so on every row.
//! Three is enough to separate a cost that quadruples from one that does not,
//! which is the only question here; it is not enough to characterise a tail, and
//! no row should be read as if it were.
//!
//! # What it does not measure
//!
//! Whether retention should be a declared clause rather than a statement. That
//! is the rest of the same criterion and it is a decision, not a number.

use std::time::Instant;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// The table sizes, spanning eight times from end to end.
///
/// Eight times is chosen so that a cost proportional to what the table keeps
/// cannot be mistaken for measurement noise: it has to move by most of that
/// factor, and a flat arm has to stay flat across it.
const SIZES: &[u64] = &[5_000, 10_000, 20_000, 40_000];

/// How many records each delete removes, at every size.
///
/// Constant on purpose — it is the independent variable held still.
const WINDOW: u64 = 1_000;

/// Where the removed window begins, inside every size in `SIZES`.
const FROM: u64 = 1_000;

/// How many times each cell is built and measured.
const REPEATS: usize = 3;

/// Fill a table with `records` records whose identity and `n` field agree.
///
/// The two carrying the same number is what makes the arms comparable: the span
/// walks the key and the condition tests the field, and they name one set.
fn build(session: &mut tessaridb::Session, table: &str, records: u64) -> Failable<()> {
    session.run(&format!("DEFINE TABLE {table} SCHEMALESS;"))?;
    for n in 0..records {
        session.run(&format!(
            "CREATE {table}:{n} = {{ n: {n}, note: 'a payload long enough to be a record rather than a token' }};"
        ))?;
    }
    Ok(())
}

/// The identities a table still holds, in the order it answers with.
fn remaining(session: &mut tessaridb::Session, table: &str) -> Failable<Vec<String>> {
    let outcomes = session.run(&format!("SELECT * FROM {table};"))?;
    Ok(outcomes
        .first()
        .and_then(tessaridb::Outcome::records)
        .map(|records| records.iter().map(|(id, _)| id.to_literal()).collect())
        .unwrap_or_default())
}

/// The same window removed two ways, at four table sizes.
///
/// # Errors
///
/// Returns whatever the store refuses.
pub(crate) fn retention(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;

    let upto = FROM.saturating_add(WINDOW);
    let mut reports = Vec::new();

    // Coldest first, and it is the arm expected to win. See the module header.
    for &size in SIZES {
        let mut samples = Samples::with_capacity(REPEATS);
        for round in 0..REPEATS {
            let table = format!("span_{size}_{round}");
            build(&mut session, &table, size)?;
            let started = Instant::now();
            session.run(&format!("DELETE FROM {table}:{FROM}..{upto} LIMIT ALL;"))?;
            samples.push(started.elapsed());
        }
        reports.push(samples.summarise(&format!("span delete, table of {size}")));
    }

    for &size in SIZES {
        let mut samples = Samples::with_capacity(REPEATS);
        for round in 0..REPEATS {
            let table = format!("cond_{size}_{round}");
            build(&mut session, &table, size)?;
            let started = Instant::now();
            session.run(&format!(
                "DELETE FROM {table} WHERE n >= {FROM} AND n < {upto} LIMIT ALL;"
            ))?;
            samples.push(started.elapsed());
        }
        reports.push(samples.summarise(&format!("conditional delete, table of {size}")));
    }

    // The half a timing table cannot speak to. Compared by what REMAINS, record
    // by record rather than by count — two deletes can leave the same number of
    // different records, and a count would report that as agreement.
    let largest = SIZES.last().copied().unwrap_or(0);
    let after_span = remaining(&mut session, &format!("span_{largest}_0"))?;
    let after_cond = remaining(&mut session, &format!("cond_{largest}_0"))?;

    reports.push(Report::measurement(
        "  records remaining",
        &format!(
            "span {} | conditional {}",
            after_span.len(),
            after_cond.len()
        ),
    ));
    reports.push(Report::measurement(
        "  the two remainders",
        if after_span == after_cond {
            "identical, compared record by record"
        } else {
            "DIFFER — the two deletes did not remove the same records"
        },
    ));
    Ok(reports)
}
