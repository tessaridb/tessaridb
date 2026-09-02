//! What a cursor is worth: the same page reached three ways, at four depths.
//!
//! # The question this exists to answer
//!
//! `AFTER <record>` is a spelling. Whether it is *worth* anything is a question
//! about the store underneath it, and the only honest answer is a measurement:
//! an offset reads every record it passes over, so its cost grows with the
//! page's position; a seek starts at a position in the keyspace, so its cost
//! does not. Two curves, one linear and one flat, and a clause that shipped
//! without them would be a claim rather than a feature.
//!
//! # Why the walked page is measured too
//!
//! Because it is the one a reader will be surprised by. A cursor over a read
//! that named its own order cannot seek — the answer's order is the key the
//! author wrote and not the store's — so it reads the records and keeps the ones
//! after the anchor, which is the work an offset does. The store says so with
//! `Note::CursorWalked`, and this is the number behind that note. Reporting it
//! beside the seek is the difference between a feature and a slogan.
//!
//! # Why the deepest page is at 99 000 and not at 100 000
//!
//! A page at the very end of the table is a page that is not there, and timing
//! an empty answer measures the walk to the end rather than the page. The
//! deepest depth is the last one that still holds a full page.

use std::time::Instant;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How many records the paged table holds.
///
/// Deep enough that the deepest page passes over most of the table, because a
/// corpus where every page is shallow measures the two paths agreeing.
const PAGED: u64 = 100_000;

/// How many records a page holds.
const PAGE: u64 = 20;

/// How many pages are timed at each depth.
const SAMPLES: usize = 20;

/// The depths a page is asked for at.
const DEPTHS: [u64; 4] = [0, 1_000, 10_000, 99_000];

/// Pages by offset, by cursor, and by a cursor that cannot seek.
///
/// # Errors
///
/// Returns whatever the store returns: this harness reports failures rather than
/// timing around them.
pub fn paging(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION paged;")?;
    for n in 0..PAGED {
        session.run(&format!(
            "CREATE paged:{n} = {{ name: 'person {n}', city: 'city {}' }};",
            n % 50
        ))?;
    }

    let mut reports = vec![Report::measurement(
        "corpus",
        &format!("{PAGED} records, pages of {PAGE}"),
    )];
    for depth in DEPTHS {
        // The offset first at each depth, so neither path is measured against a
        // cache the other one warmed.
        let mut offset = Samples::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            session.run(&format!("SELECT * FROM paged START {depth} LIMIT {PAGE};"))?;
            offset.push(started.elapsed());
        }
        reports.push(offset.summarise(&format!("offset at {depth}")));

        // The anchor is the record before the page, which is exactly what the
        // previous page's last record would have been.
        let anchor = depth.saturating_sub(1);
        let mut cursor = Samples::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            session.run(&format!(
                "SELECT * FROM paged AFTER paged:{anchor} LIMIT {PAGE};"
            ))?;
            cursor.push(started.elapsed());
        }
        reports.push(cursor.summarise(&format!("cursor at {depth}")));

        // And the cursor that cannot seek, on the same records at the same
        // depth: this is the number `Note::CursorWalked` is warning about.
        //
        // Its pair is measured beside it and not against the plain offset above,
        // because the two are not the same question: a read that names an order
        // sorts the table whether it pages by offset or by cursor, and comparing
        // a sorted page against an unsorted one would credit the cursor with the
        // sort's cost or blame it for it. The honest comparison is the same
        // statement with `START` in place of `AFTER`.
        let mut ordered = Samples::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            session.run(&format!(
                "SELECT * FROM paged ORDER BY name START {depth} LIMIT {PAGE};"
            ))?;
            ordered.push(started.elapsed());
        }
        reports.push(ordered.summarise(&format!("ordered offset at {depth}")));

        let mut walked = Samples::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            session.run(&format!(
                "SELECT * FROM paged ORDER BY name AFTER paged:{anchor} LIMIT {PAGE};"
            ))?;
            walked.push(started.elapsed());
        }
        reports.push(walked.summarise(&format!("ordered cursor (walked) at {depth}")));
    }
    Ok(reports)
}
