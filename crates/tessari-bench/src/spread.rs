//! Whether the record count's disk cost is its own version chain or the store.
//!
//! # The question, and why it is not academic
//!
//! The count the planner reads to decide whether an index is worth serving is
//! stored as an ordinary versioned record, so **every write to a table appends a
//! new version of that table's count and nothing removes the old ones**, while
//! reading the current value happens once per commit. W164 priced that step by
//! counterfactual build: in memory it is one microsecond and flat across a
//! quarter of a million records; on disk it is not flat, rising from 3.9 µs at
//! 2 500 records to 12.3 µs at 250 000.
//!
//! Two mechanisms both predict that ramp and W164 could not separate them:
//!
//! 1. **the chain** — the count record's own version chain, read once per commit
//!    and one entry longer after every write;
//! 2. **the store** — the engine simply holding more keys, so its levels and its
//!    compaction cost more for every write, the count's own included.
//!
//! Only the first has a fix inside the module. The second is the backend's, and
//! belongs in the capacity numbers rather than in anyone's list of work.
//!
//! # What separates them
//!
//! Hold the store's total key population fixed and vary only how long an
//! individual count record's chain is. One arm writes every record into a single
//! table, so its count reaches the full record count. The other spreads the same
//! number of records over a hundred tables, so each of its counts reaches a
//! hundredth of it. The store ends up holding the same keys either way.
//!
//! - under **the store**, the two arms ramp together — parallel curves;
//! - under **the chain**, the single-table arm diverges upward as the run goes on.
//!
//! # Why the arms alternate write by write
//!
//! Because a blocked comparison charges machine drift to whichever arm ran later,
//! which is how W164's first attempt concluded the term was below the noise floor
//! when it was not. Alternating at the level of a single write also removes the
//! store-state difference between the arms: each pair sees the same engine, one
//! write apart, from the first pair to the last. No counterfactual build is
//! needed and no second store is opened — the arms differ in exactly one thing.
//!
//! # The confounder, stated before the run rather than after it
//!
//! The spread arm resolves a different table per statement and its records carry
//! a hundred distinct key prefixes. Both make that arm **more** expensive, not
//! less, so the bias runs against finding the chain. A divergence that survives
//! it is real; an absence of one is weaker evidence than it looks, and is
//! reported that way.
//!
//! # What this does not measure
//!
//! What the count costs at all — that is W164's number and it stands. This
//! workload runs both arms with the count doing its ordinary work, and asks only
//! what the disk ramp is made of.

use std::time::Instant;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How many alternating rounds the workload runs.
///
/// Twenty-five rounds of ten thousand pairs put a quarter of a million records
/// in the single table, which is where W164 measured the ramp at its widest —
/// so the two runs describe the same chain length rather than similar ones.
const ROUNDS: u64 = 25;

/// How many write pairs each round performs.
///
/// One pair is one write to each arm. Ten thousand samples per arm per round
/// make a median that moves on the mechanism rather than on the scheduler.
const PAIRS: u64 = 10_000;

/// How many tables the spread arm writes across.
///
/// A hundred, so each of its count records carries a hundredth of the chain the
/// single-table arm's does while the store holds the same number of keys.
const TABLES: u64 = 100;

/// The two arms, alternating write by write.
pub fn spread(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();

    let mut definitions =
        String::from("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION one;");
    for table in 0..TABLES {
        definitions.push_str(&format!(" DEFINE COLLECTION many{table};"));
    }
    session.run(&definitions)?;

    let capacity = usize::try_from(PAIRS).unwrap_or(0);
    let mut reports = Vec::new();
    let mut written = 0_u64;
    for round in 0..ROUNDS {
        let mut single = Samples::with_capacity(capacity);
        let mut across = Samples::with_capacity(capacity);
        for _ in 0..PAIRS {
            let n = written;
            let table = n.checked_rem(TABLES).unwrap_or(0);

            let started = Instant::now();
            session.run(&format!("CREATE one:{n} = {};", payload(n)))?;
            single.push(started.elapsed());

            let started = Instant::now();
            session.run(&format!("CREATE many{table}:{n} = {};", payload(n)))?;
            across.push(started.elapsed());

            written = written.saturating_add(1);
        }
        let each = written.checked_div(TABLES).unwrap_or(0);
        reports.push(single.summarise(&format!(
            "round {} · one table, {written} records in it",
            round.saturating_add(1)
        )));
        reports.push(across.summarise(&format!(
            "round {} · a hundred tables, {each} records in each",
            round.saturating_add(1)
        )));
    }
    Ok(reports)
}

/// The record both arms write, so the only difference between them is the table.
fn payload(n: u64) -> String {
    format!(
        "{{ name: 'record {n}', city: 'city {}', \
         note: 'a line of prose long enough to be a real payload rather than a token' }}",
        n.checked_rem(100).unwrap_or(0)
    )
}
