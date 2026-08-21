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

use bgv_db::{Db, Result};

use crate::samples::{Report, Samples};

/// How many records each workload writes before reading.
///
/// Small enough that a full run finishes while somebody is watching, large
/// enough that a scan and an index read are visibly different. It is a starting
/// value, and the baseline file records which one produced its numbers.
const RECORDS: u64 = 2_000;

/// How many reads each read phase performs.
const READS: u64 = 2_000;

/// The dimension of the vectors the nearest-neighbour workload writes.
const DIMENSIONS: usize = 32;

/// One named question the harness can ask.
pub struct Workload {
    /// How it is named on the command line.
    pub name: &'static str,
    /// What the numbers it produces mean.
    pub about: &'static str,
    /// Run it against an open database.
    pub run: fn(&Db) -> Result<Vec<Report>>,
}

/// Every workload, so `--list` cannot drift from what `--workload` accepts.
pub const ALL: &[Workload] = &[
    Workload {
        name: "write",
        about: "point writes of a small record, one statement each",
        run: write,
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
        name: "search",
        about: "a term search over a full-text index, against the scan of the same condition",
        run: search,
    },
    Workload {
        name: "vector",
        about: "a nearest-neighbour read over a scan — the number an HNSW index has to beat",
        run: vector,
    },
];

/// The workload of this name, if there is one.
#[must_use]
pub fn by_name(name: &str) -> Option<&'static Workload> {
    ALL.iter().find(|workload| workload.name == name)
}

/// A namespace and database to work in.
fn prepared(db: &Db) -> Result<()> {
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

fn write(db: &Db) -> Result<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE TABLE people;")?;

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

fn read_by_id(db: &Db) -> Result<Vec<Report>> {
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

fn filter(db: &Db) -> Result<Vec<Report>> {
    let mut reports = write(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;

    // The scan first, deliberately: measuring it after the index exists would
    // measure a table the index has already warmed the cache for.
    let mut scanned = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            scanned,
            session.run(&format!(
                "SELECT * FROM people WHERE city = 'city {}';",
                n % 50
            ))?
        );
    }
    reports.push(scanned.summarise("filter-scan"));

    let mut built = Samples::with_capacity(1);
    timed!(
        built,
        session.run("DEFINE INDEX by_city ON people FIELDS city;")?
    );
    reports.push(built.summarise("filter-build-index"));

    let mut served = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            served,
            session.run(&format!(
                "SELECT * FROM people WHERE city = 'city {}';",
                n % 50
            ))?
        );
    }
    reports.push(served.summarise("filter-index"));
    Ok(reports)
}

fn search(db: &Db) -> Result<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE ANALYZER simple FILTERS lowercase;\n\
         DEFINE TABLE notes;\n\
         DEFINE FIELD body ON notes TYPE string ANALYZER simple;",
    )?;

    let mut written = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        timed!(
            written,
            session.run(&format!(
                "CREATE notes:{n} = {{ body: 'note {n} about topic {} and matter {}' }};",
                n % 40,
                n % 7
            ))?
        );
    }
    let mut reports = vec![written.summarise("search-write")];

    let mut scanned = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            scanned,
            session.run(&format!(
                "SELECT * FROM notes WHERE body MATCHES 'topic{}';",
                n % 40
            ))?
        );
    }
    reports.push(scanned.summarise("search-scan"));

    let mut built = Samples::with_capacity(1);
    timed!(
        built,
        session.run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")?
    );
    reports.push(built.summarise("search-build-index"));

    let mut served = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            served,
            session.run(&format!(
                "SELECT * FROM notes WHERE body MATCHES 'topic{}';",
                n % 40
            ))?
        );
    }
    reports.push(served.summarise("search-index"));

    let mut ranked = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            ranked,
            session.run(&format!(
                "SELECT search::score(body, 'topic{} matter{}') AS relevance FROM notes \
                 WHERE body MATCHES 'topic{}' ORDER BY relevance DESC LIMIT 10;",
                n % 40,
                n % 7,
                n % 40
            ))?
        );
    }
    reports.push(ranked.summarise("search-rank"));
    Ok(reports)
}

fn vector(db: &Db) -> Result<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE TABLE items;")?;

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

/// A deterministic vector, so two runs measure the same data.
///
/// Not random: a benchmark whose input changes between runs cannot be compared
/// with itself, which is the one thing a baseline is for.
fn embedding(n: u64) -> String {
    let mut components = String::from("[");
    for dimension in 0..DIMENSIONS {
        if dimension > 0 {
            components.push_str(", ");
        }
        let seed = n
            .wrapping_mul(2_654_435_761)
            .wrapping_add(u64::try_from(dimension).unwrap_or(0).wrapping_mul(97));
        // A value in `0.000..0.999`, written as a decimal so the parser reads it
        // as one rather than as an integer.
        let scaled = seed % 1_000;
        components.push_str(&format!("0.{scaled:03}"));
    }
    components.push(']');
    components
}

/// How many records each workload writes, for the run preamble.
#[must_use]
pub const fn records() -> u64 {
    RECORDS
}
