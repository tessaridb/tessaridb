//! A filtered nearest-neighbour read: the graph against the exact filtered read.
//!
//! Recall under a filter is a different number from unfiltered recall, and the
//! one a filtered walk is judged by: for each query the approximate ten are
//! compared with the exact ten of the SAME filtered read, at three
//! selectivities. Each record carries `band = n % 100`, so `band < s`
//! admits `s` in a hundred. The path each read took is counted too, because a
//! walk that gave the read back to the exact path is a fallback, not a recall.

use super::{FILTERED_RECORDS, Failable, QUERIES, embedding, ids, prepared};
use crate::samples::{Report, Samples};
use std::time::Instant;
use tessaridb::Db;

/// The three selectivities measured, as `band < s` out of a hundred.
const SELECTIVITIES: [u64; 3] = [50, 10, 1];

pub(crate) fn vector_filtered(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION items;")?;
    for n in 0..FILTERED_RECORDS {
        session.run(&format!(
            "CREATE items:{n} = {{ embedding: {}, band: {} }};",
            embedding(n),
            n % 100
        ))?;
    }
    session.run("DEFINE INDEX by_embedding ON items FIELDS embedding VECTOR euclidean;")?;

    let mut reports = Vec::new();
    for selectivity in SELECTIVITIES {
        let mut exact = Samples::with_capacity(QUERIES);
        let mut approximate = Samples::with_capacity(QUERIES);
        let (mut overlap, mut asked, mut walked) = (0_u64, 0_u64, 0_u64);
        for n in 0..QUERIES {
            let query = embedding(
                u64::try_from(n)
                    .unwrap_or(0)
                    .saturating_add(FILTERED_RECORDS),
            );
            let exact_read = format!(
                "SELECT * FROM items WHERE band < {selectivity} \
                 ORDER BY vector::euclidean(embedding, {query}) LIMIT 10;"
            );
            let walked_read = format!("{} APPROXIMATE;", exact_read.trim_end_matches(';'));
            let truth = ids(timed!(exact, session.run(&exact_read)?));
            let answered = timed!(approximate, session.run(&walked_read)?);
            if answered.first().and_then(tessaridb::Outcome::path)
                == Some(tessaridb::AccessPath::Approximate)
            {
                walked = walked.saturating_add(1);
            }
            let found = ids(answered);
            overlap = overlap.saturating_add(
                u64::try_from(found.iter().filter(|id| truth.contains(id)).count()).unwrap_or(0),
            );
            asked = asked.saturating_add(u64::try_from(truth.len()).unwrap_or(0));
        }
        reports.push(exact.summarise(&format!("filtered-exact {selectivity}%")));
        reports.push(approximate.summarise(&format!("filtered-walk {selectivity}%")));
        let recall = if asked == 0 {
            0.0
        } else {
            f64::from(u32::try_from(overlap).unwrap_or(0)) * 100.0
                / f64::from(u32::try_from(asked).unwrap_or(1))
        };
        reports.push(Report::measurement(
            &format!("recall {selectivity}%"),
            &format!(
                "{recall:.1}% of the exact filtered ten over {asked} asked; \
                 {walked} of {QUERIES} reads served by the walk"
            ),
        ));
    }
    Ok(reports)
}
