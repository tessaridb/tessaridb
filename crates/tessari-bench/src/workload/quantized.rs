//! A quantized vector store against a full-precision one over the same vectors.
//!
//! What a quantized index is worth is two numbers read together: what a vector
//! costs to hold, and what recall the codes still buy once a read is rescored on
//! the full vectors. Both stores hold the same records and answer the same
//! queries, and each approximate ten is compared with the exact ten.

use super::{FILTERED_RECORDS, Failable, QUERIES, embedding, ids, prepared};
use crate::samples::{Report, Samples};
use std::time::Instant;
use tessaridb::Db;

pub(crate) fn vector_quantized(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;
    let mut reports = Vec::new();
    for (store, word) in [("full", ""), ("coded", " QUANTIZED")] {
        session.run(&format!(
            "DEFINE VECTOR {store} DIMENSION {} DISTANCE euclidean{word};",
            super::DIMENSIONS
        ))?;
        // One transaction, so the graph is read and extended in one batch.
        let mut script = String::from("BEGIN;\n");
        for n in 0..FILTERED_RECORDS {
            script.push_str(&format!(
                "CREATE {store}:{n} = {{ vector: {} }};\n",
                embedding(n)
            ));
        }
        script.push_str("COMMIT;");
        let mut built = Samples::with_capacity(1);
        timed!(built, session.run(&script)?);
        reports.push(built.summarise(&format!("{store} write + build")));
        session.run(&format!("REBUILD INDEX vector ON {store};"))?;
        let info = session.run(&format!("INFO FOR VECTOR {store};"))?;
        let held = info
            .first()
            .and_then(|outcome| match outcome {
                tessaridb::Outcome::Value(tessaridb::Value::Object(fields)) => Some(
                    ["vector_bytes", "node_bytes", "nodes"]
                        .iter()
                        .map(|name| {
                            format!(
                                "{name} {}",
                                fields
                                    .get(*name)
                                    .map_or_else(String::new, ToString::to_string)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                _ => None,
            })
            .unwrap_or_default();
        reports.push(Report::measurement(&format!("{store} footprint"), &held));

        let mut exact = Samples::with_capacity(QUERIES);
        let mut walked = Samples::with_capacity(QUERIES);
        let (mut overlap, mut asked) = (0_u64, 0_u64);
        for n in 0..QUERIES {
            let query = embedding(
                u64::try_from(n)
                    .unwrap_or(0)
                    .saturating_add(FILTERED_RECORDS),
            );
            let read = format!(
                "SELECT * FROM {store} ORDER BY vector::euclidean(vector, {query}) LIMIT 10"
            );
            let truth = ids(timed!(exact, session.run(&format!("{read};"))?));
            let found = ids(timed!(
                walked,
                session.run(&format!("{read} APPROXIMATE;"))?
            ));
            overlap = overlap.saturating_add(
                u64::try_from(found.iter().filter(|id| truth.contains(id)).count()).unwrap_or(0),
            );
            asked = asked.saturating_add(u64::try_from(truth.len()).unwrap_or(0));
        }
        reports.push(exact.summarise(&format!("{store} exact")));
        reports.push(walked.summarise(&format!("{store} walk")));
        let recall = if asked == 0 {
            0.0
        } else {
            f64::from(u32::try_from(overlap).unwrap_or(0)) * 100.0
                / f64::from(u32::try_from(asked).unwrap_or(1))
        };
        reports.push(Report::measurement(
            &format!("{store} recall"),
            &format!("{recall:.1}% of the exact ten over {asked} asked"),
        ));
    }
    Ok(reports)
}
