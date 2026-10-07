//! The workloads, and what each one is a question about.
//!
//! Every workload here corresponds to a claim some earlier wave made about cost.
//! A read served by an index is *supposed* to beat the scan of the same
//! condition; a term search is supposed to be cheaper than reading every record
//! and analysing it; a nearest-neighbour read over a scan is supposed to be
//! linear, which is the number the HNSW index will have to beat. None of those
//! had a number until now, and a claim about cost with no number is a claim
//! nobody can check.
//!
//! # Why every workload writes its own data
//!
//! A shared fixture makes the phases depend on each other's ordering, and the
//! first thing anyone does with a harness is run one workload on its own. Each
//! builds what it needs and reports the build as its own phase, so the setup
//! cost is visible rather than hidden inside the measurement.

use std::time::Instant;

use tessaridb::Db;

/// What a workload can fail with.
///
/// Boxed because a workload may touch more than the database — the restore
/// rehearsal runs a backup, whose failures are its own — and a harness only ever
/// prints an error rather than deciding on one.
pub type Failable<T> = std::result::Result<T, Box<dyn std::error::Error>>;

use crate::samples::{Report, Samples};
pub(crate) use filtered::vector_filtered;
pub(crate) use heavy::{clustered, restore, vault, vector_index};
pub(crate) use planner::planner;
pub(crate) use quantized::vector_quantized;
pub(crate) use reads::{filter, search};

/// How many records each workload writes before reading.
///
/// Small enough that a full run finishes while somebody is watching, large
/// enough that a scan and an index read are visibly different. It is a starting
/// value, and the baseline file records which one produced its numbers.
const RECORDS: u64 = 2_000;

/// How many reads each read phase performs.
const READS: u64 = 2_000;

/// How many batches the capacity workload writes.
///
/// Enough to cross the memtable ceiling, because a capacity run that stops short
/// of the first flush measures a store that has not yet done the thing it will
/// spend its life doing. At the observed rate that is somewhere past a hundred
/// and fifty thousand records.
const CAPACITY_BATCHES: u64 = 100;

/// How many records each of those batches holds.
const CAPACITY_BATCH: u64 = 2_500;

/// How many nearest-neighbour queries the index workload asks.
/// How many secrets the vault workload writes and reads back.
///
/// Smaller than `RECORDS` on purpose: every one of these does an Argon2id-free
/// but still real key unwrap and an AEAD open, and a `REVEAL` additionally
/// commits its own transaction — so five hundred is already thousands of engine
/// operations, and a larger number would buy precision this row does not need.
const SECRETS: u64 = 500;

const QUERIES: usize = 100;

/// How many records the filtered nearest-neighbour workload writes: ten times
/// the others, so a walk's ceiling is a fraction of the table and the cost of a
/// selective filter shows.
const FILTERED_RECORDS: u64 = 20_000;

/// The dimension of the vectors the nearest-neighbour workload writes.
const DIMENSIONS: usize = 32;

/// One named question the harness can ask.
pub struct Workload {
    /// How it is named on the command line.
    pub name: &'static str,
    /// What the numbers it produces mean.
    pub about: &'static str,
    /// Run it against an open database.
    pub run: fn(&Db) -> Failable<Vec<Report>>,
}

/// Every workload, so `--list` cannot drift from what `--workload` accepts.
pub const ALL: &[Workload] = &[
    Workload {
        name: "write",
        about: "point writes of a small record, one statement each",
        run: write,
    },
    Workload {
        name: "concurrent",
        about: "point writes from 1 to 16 threads at once into one table — throughput, tail latency and refused commits as writers are added",
        run: crate::concurrent::concurrent,
    },
    Workload {
        name: "readers",
        about: "point reads from 1 to 16 threads at once over 20 000 records — reads per second and tail latency as readers are added",
        run: crate::readers::readers,
    },
    Workload {
        name: "read-by-id",
        about: "point reads by record identity — the cheapest access path there is",
        run: read_by_id,
    },
    Workload {
        name: "filter",
        about: "the same equality filter over a scan and over an index, so the two are one table",
        run: filter,
    },
    Workload {
        name: "range",
        about: "an index range at four widths, with what the process holds at each",
        run: crate::ranges::range,
    },
    // Only in a counting build, because without the allocator behind it every
    // figure it reports would be zero — a workload that runs and answers
    // nothing is worse than one that is absent from `--list`.
    #[cfg(feature = "counting")]
    Workload {
        name: "memory",
        about: "where an answer's memory goes: four readings around one widest read, and the same record built outside the store",
        run: crate::memory::memory,
    },
    Workload {
        name: "search",
        about: "a term search over a full-text index, against the scan of the same condition",
        run: search,
    },
    Workload {
        name: "capacity",
        about: "sustained writes in escalating batches, with p99 and resident memory per batch",
        run: capacity,
    },
    Workload {
        name: "vault",
        about: "a sealed write and a REVEAL against the same operations without a vault — what the audit's committed transaction costs a read",
        run: vault,
    },
    Workload {
        name: "restore",
        about: "a backup and the restore that replays it — the readiness row that has to be timed",
        run: restore,
    },
    Workload {
        name: "vector-index",
        about: "the same read served by a graph, with the recall it buys against the exact scan",
        run: vector_index,
    },
    Workload {
        name: "vector-filtered",
        about: "a filtered nearest read walked through the graph, with recall against the exact filtered read at three selectivities",
        run: vector_filtered,
    },
    Workload {
        name: "vector-quantized",
        about: "a quantized vector store against a full-precision one — bytes per vector, build, walk and recall after rescoring",
        run: vector_quantized,
    },
    Workload {
        name: "paging",
        about: "the same page by offset, by cursor, and by a cursor that cannot seek, at four depths",
        run: crate::paging::paging,
    },
    Workload {
        name: "vector",
        about: "a nearest-neighbour read over a scan — the number an HNSW index has to beat",
        run: vector,
    },
    Workload {
        name: "span",
        about: "the same window over identity read three ways — by span, by scan and by a value index — with the record sets compared",
        run: crate::span::span,
    },
    Workload {
        name: "retention",
        about: "the same window removed by a span and by a condition, at four table sizes — whether the cost follows what is removed or what is kept",
        run: crate::retention::retention,
    },
    Workload {
        name: "series",
        about: "the same window removed by a declared RETAIN clause and by a condition, at four table sizes — whether the clause costs what it removes or what the table keeps",
        run: crate::series::series,
    },
    Workload {
        name: "spread",
        about: "the same writes into one table and across a hundred, alternating write by write — whether the record count's disk cost is its own version chain or the store",
        run: crate::spread::spread,
    },
    Workload {
        name: "scan-guard",
        about: "a read that selects the whole table, with the planner's veto raised and lifted over one table — what the scan guard is worth",
        run: crate::guard::guard,
    },
    Workload {
        name: "planner",
        about: "a bounded equality streamed or built whole, the guard with and without statistics, and each estimate beside what its condition answers",
        run: planner,
    },
    Workload {
        name: "feeds",
        about: "what 1, 100 and 10 000 subscriptions narrowed by a condition cost per commit, beside the same feeds on the table alone",
        run: crate::feeds::feeds,
    },
    Workload {
        name: "queue",
        about: "what a claim costs behind a prefix of held and of dead-lettered records, and a drain taken one at a time against one taken in a batch",
        run: crate::queue::queue,
    },
    Workload {
        name: "update",
        about: "updates that leave every indexed field alone against updates that move one, on a table with a value and a full-text index, then a grouped aggregate",
        run: crate::updates::update,
    },
];

/// The workload of this name, if there is one.
#[must_use]
pub fn by_name(name: &str) -> Option<&'static Workload> {
    ALL.iter().find(|workload| workload.name == name)
}

/// A namespace and database to work in.
pub(crate) fn prepared(db: &Db) -> Failable<()> {
    db.session().run(
        "DEFINE NAMESPACE bench; USE NAMESPACE bench;\n\
         DEFINE DATABASE bench; USE DATABASE bench;",
    )?;
    Ok(())
}

/// A session that has already selected the tenancy.
///
/// Each operation runs its own script, because that is what a caller does — a
/// harness that batched them would measure a batching feature the language does
/// not offer.
macro_rules! timed {
    ($samples:expr, $body:expr) => {{
        let started = Instant::now();
        let outcome = $body;
        $samples.push(started.elapsed());
        outcome
    }};
}

// Declared after `timed!`, which they use: a macro is in scope only below its definition.
mod filtered;
mod heavy;
mod planner;
mod quantized;
mod reads;

fn write(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION people;")?;

    let mut samples = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        timed!(
            samples,
            session.run(&format!(
                "CREATE people:{n} = {{ name: 'person {n}', city: 'city {}', age: {} }};",
                n % 50,
                n % 90
            ))?
        );
    }
    Ok(vec![samples.summarise("write")])
}

fn read_by_id(db: &Db) -> Failable<Vec<Report>> {
    let mut reports = write(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;

    let mut samples = Samples::with_capacity(usize::try_from(READS).unwrap_or(0));
    for n in 0..READS {
        timed!(
            samples,
            session.run(&format!("SELECT * FROM people:{};", n % RECORDS))?
        );
    }
    reports.push(samples.summarise("read-by-id"));
    Ok(reports)
}

fn vector(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION items;")?;

    let mut written = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        timed!(
            written,
            session.run(&format!(
                "CREATE items:{n} = {{ embedding: {} }};",
                embedding(n)
            ))?
        );
    }
    let mut reports = vec![written.summarise("vector-write")];

    // Every read here is linear in the table. That is the point: it is the
    // number the HNSW index (SGC.T4 W2) has to beat, and without it that node's
    // acceptance would be a comparison against nothing.
    let mut nearest = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            nearest,
            session.run(&format!(
                "SELECT * FROM items ORDER BY vector::cosine(embedding, {}) LIMIT 10;",
                embedding(n)
            ))?
        );
    }
    reports.push(nearest.summarise("vector-nearest-scan"));

    Ok(reports)
}

/// How much this store takes before it stops meeting a bound, and what grows.
///
/// The readiness gate asks for **capacity measured at a latency bound with the
/// saturating resource named**, which is the one never-waived row nothing here
/// could answer. It needs three things a throughput number alone does not give:
/// a bound to be measured against, a shape over a growing store rather than a
/// single point, and something observable about what is running out.
///
/// So this writes in escalating batches and reports, per batch, the throughput,
/// the p99 and the process's resident memory. A store whose p99 climbs while
/// memory is flat is bound by the device or by compaction; one whose memory
/// climbs with it is bound by what it is holding. The numbers say which, and the
/// checklist records the reading rather than this function guessing at it.
fn capacity(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION load;")?;

    let mut reports = Vec::new();
    let mut written = 0_u64;
    for batch in 0..CAPACITY_BATCHES {
        let mut samples = Samples::with_capacity(usize::try_from(CAPACITY_BATCH).unwrap_or(0));
        for _ in 0..CAPACITY_BATCH {
            let n = written;
            timed!(
                samples,
                session.run(&format!(
                    "CREATE load:{n} = {{ name: 'record {n}', city: 'city {}', \
                     note: 'a line of prose long enough to be a real payload rather than a token' }};",
                    n % 100
                ))?
            );
            written = written.saturating_add(1);
        }
        reports.push(samples.summarise(&format!(
            "batch {} ({written} records)",
            batch.saturating_add(1)
        )));
        if let Some(resident) = resident_bytes() {
            reports.push(Report::measurement(
                "  resident",
                &format!("{} KiB after {written} records", resident / 1024),
            ));
        }
    }
    Ok(reports)
}

/// This process's resident memory, in bytes.
///
/// Read by asking the operating system's own tool rather than by linking one:
/// `ps` is on every platform this runs on, the harness is not a production path,
/// and a dependency taken to read one number would be in the tree for ever. A
/// platform where it does not answer reports nothing rather than a guess.
pub(crate) fn resident_bytes() -> Option<u64> {
    let held = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p"])
        .arg(std::process::id().to_string())
        .output()
        .ok()?;
    let text = String::from_utf8(held.stdout).ok()?;
    // `ps` reports kibibytes on both platforms this is run on.
    text.trim().parse::<u64>().ok()?.checked_mul(1024)
}

/// The record ids an answer carried.
fn ids(outcomes: Vec<tessaridb::Outcome>) -> Vec<tessari_types::RecordId> {
    outcomes
        .first()
        .and_then(|outcome| outcome.records())
        .map(|records| records.iter().map(|(id, _)| id.clone()).collect())
        .unwrap_or_default()
}

/// A deterministic vector, so two runs measure the same data.
///
/// Not random: a benchmark whose input changes between runs cannot be compared
/// with itself, which is the one thing a baseline is for.
fn embedding(n: u64) -> String {
    clustered(n)
}

/// How many records each workload writes, for the run preamble.
#[must_use]
pub const fn records() -> u64 {
    RECORDS
}
