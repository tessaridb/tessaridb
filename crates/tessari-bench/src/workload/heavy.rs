//! The workloads that cost the most to set up: vector indexes, the vault and restore.

use super::{DIMENSIONS, Failable, QUERIES, RECORDS, SECRETS, embedding, ids, prepared};
use crate::samples::{Report, Samples};
use std::time::Instant;
use tessaridb::Db;

/// A backup, and the restore that replays it.
///
/// The readiness checklist asks for a restore that is **rehearsed and timed**,
/// and a time nobody measured is neither. Both halves are one operation each
/// rather than a hundred, so the numbers are the wall time of the thing an
/// operator would actually run — a p50 over one sample is that sample, and the
/// row says `ops 1` so nobody reads it as a throughput.
pub(crate) fn restore(db: &Db) -> Failable<Vec<Report>> {
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
pub(crate) fn vector_index(db: &Db) -> Failable<Vec<Report>> {
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

/// A vector drawn near one of a few centres, the way a real embedding is.
///
/// Uniform-random points in thirty-two dimensions have no neighbourhood
/// structure at all — every pair is nearly the same distance apart — so a graph
/// index has nothing to navigate and a benchmark over them measures the curse of
/// dimensionality rather than the index. Real embeddings cluster, which is the
/// property that makes an approximate index work; so the fixture clusters too.
pub(crate) fn clustered(n: u64) -> String {
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
pub(crate) fn vault(db: &Db) -> Failable<Vec<Report>> {
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
