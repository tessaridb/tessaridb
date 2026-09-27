//! Updates that change an indexed field against updates that leave every indexed
//! field alone, and a grouped aggregate over the same table.
//!
//! The table carries the two indexes whose maintenance costs differ most: a value
//! index over `city` and a full-text index over `body`. An update that only moves
//! the unindexed counter leaves both indexes holding exactly what they held, so the
//! difference between the two update phases is what index maintenance costs when
//! nothing about an index changed — the number a skip-unchanged-index mechanism
//! would have to move.

use std::time::Instant;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// Records in the table, and updates per phase.
const RECORDS: u64 = 2_000;

/// Distinct cities, so the grouped aggregate answers this many groups.
const CITIES: u64 = 50;

/// The text every record's body starts from: long enough that analysing it is a
/// real cost, as a document body is.
const BODY: &str = "a note that carries enough words to make analysing it cost something \
                    comparable to a short paragraph written by a person about a topic";

/// Write the table, then time the two kinds of update and the aggregate.
pub fn update(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run(
        "USE NAMESPACE bench; USE DATABASE bench;\n\
         DEFINE ANALYZER plain FILTERS lowercase, ascii, stemmer;\n\
         DEFINE TABLE docs SCHEMALESS;\n\
         DEFINE FIELD body ON docs TYPE string ANALYZER plain;\n\
         DEFINE INDEX by_city ON docs FIELDS city;\n\
         DEFINE INDEX by_body ON docs FIELDS body SEARCH;",
    )?;
    for n in 0..RECORDS {
        session.run(&format!(
            "CREATE docs:{n} = {{ body: '{BODY} {n}', city: 'city {}', seen: 0 }};",
            n % CITIES
        ))?;
    }

    let mut unindexed = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        let started = Instant::now();
        session.run(&format!("UPDATE docs:{n} SET seen = seen + 1;"))?;
        unindexed.push(started.elapsed());
    }

    let mut indexed = Samples::with_capacity(usize::try_from(RECORDS).unwrap_or(0));
    for n in 0..RECORDS {
        let started = Instant::now();
        session.run(&format!(
            "UPDATE docs:{n} SET city = 'city {}';",
            n.wrapping_add(1) % CITIES
        ))?;
        indexed.push(started.elapsed());
    }

    let mut grouped = Samples::with_capacity(100);
    for _ in 0..100 {
        let started = Instant::now();
        session.run("SELECT city, count(*) AS n, mean(seen) AS seen FROM docs GROUP BY city;")?;
        grouped.push(started.elapsed());
    }

    Ok(vec![
        unindexed.summarise("update-unindexed-field"),
        indexed.summarise("update-indexed-field"),
        grouped.summarise("aggregate-group-by"),
    ])
}
