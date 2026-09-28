//! The read workloads: an equality filter and a term search, each against its scan.

use super::write;
use super::{Failable, RECORDS, prepared};
use crate::samples::{Report, Samples};
use std::time::Instant;
use tessaridb::Db;

pub(crate) fn filter(db: &Db) -> Failable<Vec<Report>> {
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

pub(crate) fn search(db: &Db) -> Failable<Vec<Report>> {
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
