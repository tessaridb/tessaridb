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
        name: "paging",
        about: "the same page by offset, by cursor, and by a cursor that cannot seek, at four depths",
        run: crate::paging::paging,
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

fn filter(db: &Db) -> Failable<Vec<Report>> {
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

    // The same shape for an ordered range: the scan first, so the index does not
    // measure a table it has already warmed.
    let mut ranged = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            ranged,
            session.run(&format!(
                "SELECT * FROM people WHERE age >= {} AND age < {};",
                n % 80_u64,
                (n % 80_u64).saturating_add(5)
            ))?
        );
    }
    reports.push(ranged.summarise("range-scan"));

    let mut bounded = Samples::with_capacity(1);
    timed!(
        bounded,
        session.run("DEFINE INDEX by_age ON people FIELDS age;")?
    );
    reports.push(bounded.summarise("range-build-index"));

    let mut walked = Samples::with_capacity(100);
    for n in 0..100 {
        timed!(
            walked,
            session.run(&format!(
                "SELECT * FROM people WHERE age >= {} AND age < {};",
                n % 80_u64,
                (n % 80_u64).saturating_add(5)
            ))?
        );
    }
    reports.push(walked.summarise("range-index"));
    Ok(reports)
}

fn search(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE ANALYZER simple FILTERS lowercase;\n\
         DEFINE TABLE notes SCHEMALESS;\n\
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

/// A backup, and the restore that replays it.
///
/// The readiness checklist asks for a restore that is **rehearsed and timed**,
/// and a time nobody measured is neither. Both halves are one operation each
/// rather than a hundred, so the numbers are the wall time of the thing an
/// operator would actually run — a p50 over one sample is that sample, and the
/// row says `ops 1` so nobody reads it as a throughput.
fn restore(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE COLLECTION people;\n\
         DEFINE INDEX by_city ON people FIELDS city;",
    )?;
    for n in 0..RECORDS {
        session.run(&format!(
            "CREATE people:{n} = {{ name: 'person {n}', city: 'city {}' }};",
            n % 50
        ))?;
    }

    let mut taken = Samples::with_capacity(1);
    let mut held = Vec::new();
    let written = timed!(taken, tessari_backup::write(db.store(), &mut held))?;
    let mut reports = vec![taken.summarise("backup")];
    reports.push(Report::measurement(
        "backup size",
        &format!("{} record(s), {} bytes", written.records, held.len()),
    ));

    // Into a fresh store, because a restore into anything else is refused.
    let target = Db::in_memory()?;
    let mut replayed = Samples::with_capacity(1);
    let outcome = timed!(
        replayed,
        tessari_backup::read(target.store(), &mut held.as_slice())
    )?;
    reports.push(replayed.summarise("restore"));
    reports.push(Report::measurement(
        "restored",
        &format!(
            "{} record(s), truncated: {}",
            outcome.records, outcome.truncated
        ),
    ));
    Ok(reports)
}

/// The graph, against the scan it is meant to replace.
///
/// Recall is **measured** rather than asserted: for each query the approximate
/// ten are compared against the exact ten of the same read, and the overlap is
/// reported as a percentage. That the exact answer is available at all is what
/// makes this index testable — the scan's ten *are* the right ten, so there is
/// nothing to argue about.
fn vector_index(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION items;")?;
    for n in 0..RECORDS {
        session.run(&format!(
            "CREATE items:{n} = {{ embedding: {} }};",
            embedding(n)
        ))?;
    }

    let mut built = Samples::with_capacity(1);
    timed!(
        built,
        session.run("DEFINE INDEX by_embedding ON items FIELDS embedding VECTOR euclidean;")?
    );
    let mut reports = vec![built.summarise("vector-index-build")];

    let mut exact = Samples::with_capacity(QUERIES);
    let mut approximate = Samples::with_capacity(QUERIES);
    let mut overlap = 0_u64;
    let mut asked = 0_u64;
    for n in 0..QUERIES {
        let query = embedding(u64::try_from(n).unwrap_or(0).saturating_add(RECORDS));
        let exact_read =
            format!("SELECT * FROM items ORDER BY vector::euclidean(embedding, {query}) LIMIT 10;");
        let walked_read = format!("{} APPROXIMATE;", exact_read.trim_end_matches(';'));

        let truth = ids(timed!(exact, session.run(&exact_read)?));
        let found = ids(timed!(approximate, session.run(&walked_read)?));
        overlap = overlap.saturating_add(
            u64::try_from(found.iter().filter(|id| truth.contains(id)).count()).unwrap_or(0),
        );
        asked = asked.saturating_add(u64::try_from(truth.len()).unwrap_or(0));
    }
    reports.push(exact.summarise("vector-exact-scan"));
    reports.push(approximate.summarise("vector-graph-walk"));

    // The number this node's acceptance names, and the only one here that is not
    // a latency — so it is written as what it is.
    let recall = if asked == 0 {
        0.0
    } else {
        f64::from(u32::try_from(overlap).unwrap_or(0)) * 100.0
            / f64::from(u32::try_from(asked).unwrap_or(1))
    };
    reports.push(Report::measurement(
        "recall",
        &format!("{recall:.1}% of the exact ten, over {asked} asked"),
    ));
    Ok(reports)
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

/// A vector drawn near one of a few centres, the way a real embedding is.
///
/// Uniform-random points in thirty-two dimensions have no neighbourhood
/// structure at all — every pair is nearly the same distance apart — so a graph
/// index has nothing to navigate and a benchmark over them measures the curse of
/// dimensionality rather than the index. Real embeddings cluster, which is the
/// property that makes an approximate index work; so the fixture clusters too.
fn clustered(n: u64) -> String {
    const CENTRES: u64 = 40;
    let centre = n % CENTRES;
    let mut components = String::from("[");
    for dimension in 0..DIMENSIONS {
        if dimension > 0 {
            components.push_str(", ");
        }
        let axis = u64::try_from(dimension).unwrap_or(0);
        // The centre decides most of each component; the record's own identity
        // moves it a little.
        // The jitter is wide and well mixed on purpose. An earlier version took
        // it modulo sixty, which made thousands of records share a vector
        // exactly — and recall measured over duplicates is a measurement of
        // which tie a sort broke, not of whether a search found anything. It
        // read as a broken index until the fixture was looked at.
        let base = centre
            .wrapping_mul(7_919)
            .wrapping_add(axis.wrapping_mul(104_729))
            % 1_000;
        let mut mixed = n
            .wrapping_add(1)
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(axis.wrapping_mul(1_442_695_040_888_963_407));
        mixed ^= mixed >> 33;
        mixed = mixed.wrapping_mul(0xff51_afd7_ed55_8ccd);
        mixed ^= mixed >> 29;
        let held = base.wrapping_add(mixed % 200).wrapping_sub(100) % 1_000;
        components.push_str(&format!("0.{held:03}"));
    }
    components.push(']');
    components
}

/// How many records each workload writes, for the run preamble.
#[must_use]
pub const fn records() -> u64 {
    RECORDS
}

/// What a secret costs to write and to read, beside the same shapes with no
/// vault under them.
///
/// # Why four phases and not one number
///
/// `REVEAL` does three things a point read does not: it unwraps two keys, it
/// opens an AEAD envelope, and it writes an audit record **in its own committed
/// transaction** before the answer leaves. The third is a write on a read path.
/// It was a deliberate choice and it has never been measured, and the readiness
/// checklist asks the question directly: is one committed transaction per read
/// the ceiling?
///
/// A run with the trail switched off would answer it in one line and is not
/// available — the built-in device is unconditional, and adding a switch to
/// measure it would be a feature nobody asked for. So the cost is attributed
/// instead: if `vault-reveal` lands near `plain-read` plus `plain-write`, the
/// commit dominates and the answer is yes. If the remainder dominates, it is the
/// cryptography, and the audit write is not the ceiling. Both results are worth
/// having, which is the test of whether a measurement was worth taking.
fn vault(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         UNSEAL VAULT WITH 'a benchmark passphrase';\n\
         DEFINE VAULT credentials;\n\
         DEFINE FIELD login ON credentials TYPE string;\n\
         DEFINE FIELD token ON credentials TYPE string SECRET;\n\
         DEFINE COLLECTION plain;",
    )?;

    let held = usize::try_from(SECRETS).unwrap_or(0);

    let mut sealed = Samples::with_capacity(held);
    for n in 0..SECRETS {
        timed!(
            sealed,
            session.run(&format!(
                "CREATE credentials:{n} = {{ login: 'user {n}', token: 'ghp_{n}_0123456789abcdef' }};"
            ))?
        );
    }

    // The same record, the same statement shape, no vault beneath it. The
    // difference between this and the phase above is the sealing.
    let mut unsealed = Samples::with_capacity(held);
    for n in 0..SECRETS {
        timed!(
            unsealed,
            session.run(&format!(
                "CREATE plain:{n} = {{ login: 'user {n}', token: 'ghp_{n}_0123456789abcdef' }};"
            ))?
        );
    }

    let mut revealed = Samples::with_capacity(held);
    for n in 0..SECRETS {
        timed!(
            revealed,
            session.run(&format!("REVEAL token FROM credentials:{n};"))?
        );
    }

    let mut read = Samples::with_capacity(held);
    for n in 0..SECRETS {
        timed!(read, session.run(&format!("SELECT * FROM plain:{n};"))?);
    }

    Ok(vec![
        sealed.summarise("vault-write"),
        unsealed.summarise("plain-write"),
        revealed.summarise("vault-reveal"),
        read.summarise("plain-read"),
    ])
}
