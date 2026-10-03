//! What the planner's statistics are worth, and how far its estimates miss.
//!
//! Three questions over one table of fifty thousand records and an unindexed
//! mirror holding the same values under the same identities:
//!
//! - **a bounded equality read** — `band = 3 LIMIT 10`, which a tenth of the
//!   table matches. An equality read that builds its whole candidate set pays
//!   for five thousand records to answer ten; one that streams pays for ten.
//! - **the guard's probe** — `n > 0` selects the whole table, and the planner
//!   declines the index either by counting it (a probe walked to half the
//!   table) or by reading an estimate. The two arms differ by one `ANALYZE`.
//! - **the estimates themselves** — `EXPLAIN` for a common value, a rare value,
//!   a value a tenth of the table holds and three range widths, beside the
//!   count each condition actually answers.
//!
//! A build without `ANALYZE` reports that and measures the arms it can, which
//! is what makes the same workload the baseline for the change it measures.

use std::time::Instant;

use tessaridb::{Db, Outcome, Value};

use super::{Failable, prepared};
use crate::samples::{Report, Samples};

/// How many records each table holds — far above the planner's scan floor.
const RECORDS: u64 = 50_000;

/// How many times a bounded read is asked.
const BOUNDED: usize = 200;

/// How many times a read answering thousands of records is asked.
const WIDE: usize = 20;

/// The conditions whose estimates are compared with what they answer.
const ESTIMATED: [&str; 6] = [
    "skew = 0",
    "skew = 17",
    "band = 3",
    "n > 49000",
    "n > 40000",
    "n > 10000",
];

pub(crate) fn planner(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE COLLECTION people; DEFINE COLLECTION plain;",
    )?;
    let mut written = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        // Four records in ten share `skew = 0`; the rest spread over 997 values.
        let skew = if n % 10 < 4 { 0 } else { n % 997 };
        let body = format!("{{ band: {}, skew: {skew}, n: {n} }}", n % 10);
        timed!(
            written,
            session.run(&format!(
                "CREATE people:{n} = {body}; CREATE plain:{n} = {body};"
            ))?
        );
    }
    let mut reports = vec![written.summarise("planner-write")];
    session.run(
        "DEFINE INDEX by_band ON people FIELDS band;\n\
         DEFINE INDEX by_skew ON people FIELDS skew;\n\
         DEFINE INDEX by_n ON people FIELDS n;",
    )?;

    // A bounded equality: the index arm and the mirror's scan.
    let bounded = "WHERE band = 3 LIMIT 10;";
    let (indexed, scanned) = paired(&mut session, bounded, BOUNDED)?;
    reports.push(indexed.0.summarise("eq-limit-index"));
    reports.push(scanned.0.summarise("eq-limit-scan"));
    reports.push(Report::measurement(
        "eq-limit served by",
        &format!("index arm: {} | mirror: {}", indexed.1, scanned.1),
    ));

    // The same equality unbounded: five thousand records either way.
    let (indexed, scanned) = paired(&mut session, "WHERE band = 3;", WIDE)?;
    reports.push(indexed.0.summarise("eq-index"));
    reports.push(scanned.0.summarise("eq-scan"));
    reports.push(Report::measurement(
        "eq served by",
        &format!("index arm: {} | mirror: {}", indexed.1, scanned.1),
    ));

    // The guard, before any statistics: whatever the planner does without them.
    let mut probed = Samples::with_capacity(WIDE);
    for _ in 0..WIDE {
        timed!(probed, session.run("SELECT * FROM people WHERE n > 0;")?);
    }
    reports.push(probed.summarise("guard-without-statistics"));

    let analysed = session.run("ANALYZE TABLE people;").is_ok();
    if analysed {
        let mut estimated = Samples::with_capacity(WIDE);
        let mut path = "none";
        for _ in 0..WIDE {
            let answered = timed!(estimated, session.run("SELECT * FROM people WHERE n > 0;")?);
            path = served_by(&answered);
        }
        reports.push(estimated.summarise("guard-with-statistics"));
        reports.push(Report::measurement("guard-with-statistics served by", path));
        // The bounded equality again, now that the planner need not count.
        let (indexed, _) = paired(&mut session, bounded, BOUNDED)?;
        reports.push(indexed.0.summarise("eq-limit-index-with-statistics"));
    } else {
        reports.push(Report::measurement(
            "statistics",
            "this build has no ANALYZE — the estimated arms were not run",
        ));
    }

    for condition in ESTIMATED {
        let explained = session.run(&format!("EXPLAIN SELECT * FROM people WHERE {condition};"))?;
        let actual = session
            .run(&format!("SELECT * FROM plain WHERE {condition};"))?
            .last()
            .map_or(0, |outcome| match outcome {
                Outcome::Records { records, .. } => records.len(),
                _ => 0,
            });
        reports.push(Report::measurement(
            &format!("estimate {condition}"),
            &estimate_against(explained.last(), actual),
        ));
    }
    Ok(reports)
}

/// One arm's timings, and the access path its reads reported.
type Arm = (Samples, &'static str);

/// One read over the indexed table and the same read over the mirror, asked
/// alternately so drift during the run reaches both arms; the answers are
/// compared by identity every time, because the two tables share them.
fn paired(session: &mut tessaridb::Session<'_>, rest: &str, times: usize) -> Failable<(Arm, Arm)> {
    let mut indexed = Samples::with_capacity(times);
    let mut scanned = Samples::with_capacity(times);
    let (mut indexed_path, mut scanned_path) = ("none", "none");
    for _ in 0..times {
        let one = timed!(
            indexed,
            session.run(&format!("SELECT * FROM people {rest}"))?
        );
        let other = timed!(
            scanned,
            session.run(&format!("SELECT * FROM plain {rest}"))?
        );
        if identities(&one) != identities(&other) {
            return Err(format!("the two tables answered `{rest}` differently").into());
        }
        indexed_path = served_by(&one);
        scanned_path = served_by(&other);
    }
    Ok(((indexed, indexed_path), (scanned, scanned_path)))
}

fn identities(outcomes: &[Outcome]) -> Vec<String> {
    match outcomes.last() {
        Some(Outcome::Records { records, .. }) => {
            records.iter().map(|(id, _)| id.to_string()).collect()
        }
        _ => Vec::new(),
    }
}

fn served_by(outcomes: &[Outcome]) -> &'static str {
    match outcomes.last() {
        Some(Outcome::Records { plan, .. }) => plan.access.name(),
        _ => "none",
    }
}

/// What `EXPLAIN` estimated beside what the condition answered.
fn estimate_against(explained: Option<&Outcome>, actual: usize) -> String {
    let Some(Outcome::Value(Value::Object(plan))) = explained else {
        return format!("no plan; actual {actual}");
    };
    let field = |name: &str| {
        plan.get(name)
            .map_or_else(|| "-".to_owned(), ToString::to_string)
    };
    let access = field("access");
    let Some(Value::Number(number)) = plan.get("estimate") else {
        return format!("access {access}, no estimate; actual {actual}");
    };
    let estimate = number.to_string();
    let error = estimate
        .parse::<f64>()
        .ok()
        .filter(|held| *held > 0.0)
        .map_or_else(
            || "-".to_owned(),
            |held| {
                let real = f64::from(u32::try_from(actual).unwrap_or(u32::MAX));
                format!("{:.2}x", real / held)
            },
        );
    format!(
        "access {access}, estimate {estimate} by {} | actual {actual} | actual/estimate {error}",
        field("estimated_by")
    )
}
