//! `INFO FOR VECTOR` — the recall a store was **measured** at, or nothing.
//!
//! # Why this is the only number the report may carry
//!
//! A vector index answers approximately. There is a closed-form intuition
//! relating the neighbour count and the exploration budget to an expected
//! recall, and a report that printed it would look exactly like this one: a
//! percentage, beside a store, in a description. It would be a number nobody
//! checked wearing the name of one somebody did, and a reader has no way to tell
//! the two apart from the outside — which is why the distinction is held here,
//! in the one place the difference is observable.
//!
//! So the two states this file asserts are the whole of it:
//!
//! - **Nothing measured** reads as absent, and stays absent however long the
//!   store is written to. A declared, filled, never-rebuilt store reports
//!   `NONE`, and that is honest rather than a gap.
//! - **Something measured** reads as a figure that carries what it was measured
//!   over. Never the percentage alone: recall decays as records arrive after the
//!   build, so a lone number goes stale in silence — the same failure by the
//!   other door. `records` is what makes the staleness visible, which the last
//!   test here exercises directly by growing the store past its own figure.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE library; USE DATABASE library;
";

/// How many records the store is filled with before it is measured.
const FILLED: i64 = 30;

/// A session holding a three-wide euclidean store called `embeddings`.
fn declared(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "{PLACE}DEFINE VECTOR embeddings DIMENSION 3 DISTANCE euclidean;"
        ))
        .unwrap();
    session
}

/// Write `count` records whose vectors spread along a line.
fn fill(session: &mut Session<'_>, from: i64, count: i64) {
    for n in from..from.saturating_add(count) {
        let x = f64::from(i32::try_from(n).unwrap());
        session
            .run(&format!(
                "CREATE embeddings:{n} = {{ vector: [{x}, {}, 0.5] }};",
                x / 2.0
            ))
            .unwrap();
    }
}

/// The `recall` field of `INFO FOR VECTOR embeddings`.
fn recall(session: &mut Session<'_>) -> Value {
    let report = match session
        .run("INFO FOR VECTOR embeddings;")
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    };
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    fields.get("recall").cloned().unwrap()
}

/// One field of a measured recall.
fn field(measured: &Value, name: &str) -> Value {
    let Value::Object(fields) = measured else {
        panic!("the recall was not a measurement: {measured:?}");
    };
    fields.get(name).cloned().unwrap()
}

#[test]
fn a_store_nobody_has_measured_reports_nothing_however_full_it_is() {
    // The state that must not quietly become a number. Thirty records went in
    // through the maintained path, which knows each record and never the whole
    // collection — so there is nothing to measure a walk against, and the report
    // says so instead of estimating.
    let store = store();
    let mut session = declared(&store);
    fill(&mut session, 0, FILLED);

    assert_eq!(recall(&mut session), Value::None);
}

#[test]
fn a_rebuild_measures_the_store_and_the_report_says_what_it_measured() {
    // The other state. `REBUILD INDEX` walks every row, so it is the one moment
    // the whole graph and every stored vector are in hand at once — and it is a
    // logged statement, so every replica computes the same figure at the same
    // sequence rather than each on its own reckoning.
    let store = store();
    let mut session = declared(&store);
    fill(&mut session, 0, FILLED);
    session.run("REBUILD INDEX vector ON embeddings;").unwrap();

    let measured = recall(&mut session);
    assert_ne!(measured, Value::None, "a rebuild measured nothing");

    // Thirty points on a line, well inside the neighbour count, so the walk is
    // exact and the only honest figure is a hundred. Asserted rather than
    // bounded: over data this small anything less means the measurement is
    // comparing the wrong two sets.
    assert_eq!(field(&measured, "recall"), Value::from(100));
    assert_eq!(field(&measured, "at"), Value::from(10));
    assert_eq!(field(&measured, "records"), Value::from(FILLED));
    assert_eq!(field(&measured, "sample"), Value::from(FILLED));
    // The engine constants in force, because a recall measured at one budget
    // does not describe another.
    assert_eq!(field(&measured, "neighbours"), Value::from(16));
    assert_eq!(field(&measured, "exploration"), Value::from(64));
}

#[test]
fn the_figure_keeps_saying_how_large_the_store_was_when_it_was_taken() {
    // Why `records` is stored beside the percentage. Recall decays as records
    // arrive after the build that measured it — the graph keeps answering, and
    // answers less of the truth. A bare percentage would go on looking current
    // forever; this one carries the size it was taken at, so a reader comparing
    // it against a store three times larger can see the number has been outgrown
    // without the store having to guess when it stopped being true.
    let store = store();
    let mut session = declared(&store);
    fill(&mut session, 0, FILLED);
    session.run("REBUILD INDEX vector ON embeddings;").unwrap();
    fill(&mut session, FILLED, FILLED.saturating_mul(2));

    let measured = recall(&mut session);
    assert_eq!(
        field(&measured, "records"),
        Value::from(FILLED),
        "the figure moved without anything measuring again"
    );
}

#[test]
fn a_second_rebuild_publishes_a_figure_for_the_graph_that_now_exists() {
    // The store grew and was measured again. This much a rebuild gets right
    // simply by writing to the same key — the test below is the one that does
    // not.
    let store = store();
    let mut session = declared(&store);
    fill(&mut session, 0, FILLED);
    session.run("REBUILD INDEX vector ON embeddings;").unwrap();
    fill(&mut session, FILLED, FILLED.saturating_mul(2));
    session.run("REBUILD INDEX vector ON embeddings;").unwrap();

    let measured = recall(&mut session);
    assert_eq!(
        field(&measured, "records"),
        Value::from(FILLED.saturating_mul(3)),
        "the second rebuild left the first measurement in place"
    );
}

#[test]
fn a_rebuild_with_nothing_left_to_measure_takes_the_old_figure_away() {
    // The row with teeth, and the only shape that has them. A rebuild that
    // *produces* a figure overwrites the old one at the same key, so it proves
    // nothing about clearing; a rebuild that produces **none** proves everything,
    // because the old figure survives unless the index's keyspace is cleared with
    // its entries.
    //
    // Emptied down to a single record there is no answer a walk could get wrong,
    // so the honest report is `NONE` again. Leaving ninety-nine per cent standing
    // over one record is the stale number this field exists to prevent, arriving
    // from inside — and it fails no test until somebody reads it.
    let store = store();
    let mut session = declared(&store);
    fill(&mut session, 0, FILLED);
    session.run("REBUILD INDEX vector ON embeddings;").unwrap();
    assert_ne!(recall(&mut session), Value::None, "nothing was measured");

    for n in 1..FILLED {
        session.run(&format!("DELETE embeddings:{n};")).unwrap();
    }
    session.run("REBUILD INDEX vector ON embeddings;").unwrap();

    assert_eq!(
        recall(&mut session),
        Value::None,
        "a figure survived the graph it described"
    );
}
