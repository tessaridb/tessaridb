//! An index range at escalating widths, and what holding it costs.
//!
//! # Why this is not a phase of the `filter` workload
//!
//! `filter` already times a range served by an index, over five of ninety age
//! values across two thousand records — about a hundred and twenty entries. That
//! is the right width for the question `filter` asks, which is whether an index
//! beats the scan of the same condition. It is the wrong width for this one:
//! a read that fits in a single fetch cannot show what fetching in several
//! costs, and no number taken at that width can move either way when the shape
//! of the fetch changes.
//!
//! So this reads the **same** condition at four widths spanning three orders of
//! magnitude, and reports what the process is holding at each.
//!
//! # Why the widths escalate rather than being one number
//!
//! The same reason `capacity` writes in escalating batches. A cost that is
//! linear and a cost that stops being linear are indistinguishable at one point,
//! and the interesting thing about a range read is exactly where the second
//! begins.
//!
//! # What the resident figure means, and what it does not
//!
//! It is the whole process's resident set, read from the operating system, so it
//! carries the store as well as the read. The allocator does not return freed
//! pages promptly either, which makes the sequence a **high-water mark** rather
//! than four independent measurements — each width's figure includes whatever
//! the previous one peaked at. That is why the delta from the pre-read reading
//! is reported beside it, and why the figure worth comparing between two runs is
//! the one after the widest read.
//!
//! # Why the read count falls as the width grows
//!
//! Each width touches roughly the same number of records, so no single width
//! dominates the run's wall time. The consequence is stated rather than hidden:
//! the widest rows summarise ten samples, and a ninety-ninth percentile over ten
//! samples is the slowest of them. The `ops` column says so on every row.

use std::time::Instant;

use bgv_db::{Db, Outcome};

use crate::samples::{Report, Samples};
use crate::workload::{Failable, resident_bytes};

/// How many records the range workload writes.
///
/// Large enough that the widest read is far past any plausible fetch size, and
/// small enough that writing them is a couple of seconds even on disk.
const RECORDS: u64 = 50_000;

/// The widths a range is read at: a slice, a tenth, a half, the whole table.
const WIDTHS: &[u64] = &[100, 5_000, 25_000, 50_000];

/// About how many records each width should touch in total.
///
/// The read count is derived from this so that a run's wall time is not decided
/// by its widest phase alone.
const TOUCHED: u64 = 500_000;

/// The most reads any one width performs.
const MOST_READS: u64 = 100;

/// The fewest, so even the widest row rests on more than a single sample.
const FEWEST_READS: u64 = 10;

/// An index range read at escalating widths.
///
/// # Errors
///
/// Returns an error when a statement fails or the store cannot be prepared.
pub fn range(db: &Db) -> Failable<Vec<Report>> {
    let mut session = db.session();
    session.run(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench;\n\
         DEFINE DATABASE bench; USE DATABASE bench;\n\
         DEFINE TABLE spans;\n\
         DEFINE INDEX by_n ON spans FIELDS n;",
    )?;

    let mut written = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        let started = Instant::now();
        session.run(&format!(
            "CREATE spans:{n} = {{ n: {n}, \
             note: 'a line of prose long enough to be a real payload rather than a token' }};"
        ))?;
        written.push(started.elapsed());
    }
    let mut reports = vec![written.summarise("range-write")];

    // Read before any range read, so every later figure has something to be a
    // delta from. Taken after the writes, because the store is not what this
    // workload is asking about.
    let settled = resident_bytes();
    if let Some(held) = settled {
        reports.push(Report::measurement(
            "  resident before",
            &format!("{} KiB", held / 1024),
        ));
    }

    for width in WIDTHS.iter().copied() {
        reports.extend(at_width(db, width, settled)?);
    }
    Ok(reports)
}

/// One width: the reads, what they answered, and what the process was holding.
fn at_width(db: &Db, width: u64, settled: Option<u64>) -> Failable<Vec<Report>> {
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;

    let reads = TOUCHED
        .checked_div(width)
        .unwrap_or(MOST_READS)
        .clamp(FEWEST_READS, MOST_READS);
    // Each read starts somewhere else in the table, so no two consecutive reads
    // ask for the range the last one just resolved.
    let step = RECORDS
        .saturating_sub(width)
        .checked_div(reads)
        .unwrap_or(0);

    let mut taken = Samples::with_capacity(usize::try_from(reads).unwrap_or(0));
    let mut answered = 0_usize;
    let mut path = None;
    for n in 0..reads {
        let from = n.saturating_mul(step);
        let until = from.saturating_add(width);
        let started = Instant::now();
        let outcome = session.run(&format!(
            "SELECT * FROM spans WHERE n >= {from} AND n < {until};"
        ))?;
        taken.push(started.elapsed());
        if let Some(Outcome::Records {
            records,
            path: took,
        }) = outcome.last()
        {
            answered = records.len();
            path = Some(took.name());
        }
    }

    let mut reports = vec![taken.summarise(&format!("range-{width}"))];
    // The access path is reported because it is the difference between this
    // phase measuring what it claims and it measuring the scan. A planner change
    // that stopped serving the range would otherwise leave the numbers looking
    // like a regression in the range read rather than the loss of one.
    reports.push(Report::measurement(
        "  served by",
        &format!(
            "{} — {answered} record(s) of {width} asked",
            path.unwrap_or("nothing")
        ),
    ));
    if let Some(held) = resident_bytes() {
        let grown = held.saturating_sub(settled.unwrap_or(held));
        reports.push(Report::measurement(
            "  resident",
            &format!(
                "{} KiB (+{} KiB since before the reads)",
                held / 1024,
                grown / 1024
            ),
        ));
    }
    Ok(reports)
}
