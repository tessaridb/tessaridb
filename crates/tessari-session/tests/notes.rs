//! Notes — what the store did, said without being asked.
//!
//! A read has had two channels since it existed: the records, and the refusal.
//! Neither can carry "this answer is correct and there is something about it you
//! would want to know", because an error refuses an answer that is right and the
//! records say nothing about how they were reached. So the third case has been
//! silence, and silence is what turns a fallback into folklore — the operator
//! finds out an index stopped serving a read by noticing the read got slow.
//!
//! # What every test here asserts twice
//!
//! Each one checks the note **and** the answer. That pairing is the contract: a
//! note never changes what a statement answers, so a caller that ignores every
//! note gets exactly the records it would have got before notes existed. A test
//! that only checked the note would pass on an implementation that reported
//! honestly and answered wrongly.
//!
//! # And what the negative tests are for
//!
//! Half this file asserts that no note appears. That is not padding: the failure
//! mode of a diagnostic channel is not saying too little, it is saying so much
//! that nobody reads it. `plan::ordered` reads the *statement* and never the
//! schema, so it says "this is a bounded ordered read" for an `ORDER BY … LIMIT`
//! over a table with no index at all — and a fell-back note keyed on that would
//! fire on the most ordinary read in the language. The note is keyed on an index
//! that **existed and declined**, which is what `Walked` exists to keep separate
//! from an index that was never there.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE events;",
        )
        .unwrap();
    session
}

/// A table whose condition matches one record in a hundred over the order.
///
/// The selectivity is the whole point: filling a bound of ten from a one-in-a-
/// hundred condition needs about a thousand entries, which is past the walk's
/// reach, so the read gives the order up. The numbers are the ones
/// `ordered_under_a_where` established, at a quarter of the records — enough to
/// pass the ceiling and no more.
fn populate(session: &mut Session<'_>, records: u32) {
    let mut script = String::new();
    for n in 1..=records {
        script.push_str(&format!(
            "CREATE events:{n} = {{ at: {}, rare: {}, slot: {} }};\n",
            n / 2,
            n % 100,
            n % 2
        ));
        if n % 200 == 0 {
            session.run(&script).unwrap();
            script.clear();
        }
    }
    if !script.is_empty() {
        session.run(&script).unwrap();
    }
}

/// The answer and its notes, together, because every test here needs both.
fn answered(session: &mut Session<'_>, script: &str) -> (Vec<RecordId>, Vec<Note>, AccessPath) {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Records {
        records,
        plan,
        notes,
    }) = outcomes.last()
    else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        notes.clone(),
        plan.access,
    )
}

const INDEX: &str = "DEFINE INDEX by_at ON events FIELDS at;";
const THIN: &str = "SELECT * FROM events WHERE rare = 0 ORDER BY at DESC LIMIT 10;";

#[test]
fn an_index_that_could_not_fill_the_bound_says_so() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 1000);

    // Without the index there is nothing to fall back *from*, and the read is a
    // scan that gave nothing up.
    let (without, notes, path) = answered(&mut session, THIN);
    assert_eq!(path, AccessPath::Scan);
    assert!(notes.is_empty(), "a read with no index raised {notes:?}");

    session.run(INDEX).unwrap();

    let (with, notes, path) = answered(&mut session, THIN);
    assert_eq!(path, AccessPath::Scan, "the walk filled a bound it cannot");
    assert_eq!(
        notes,
        vec![Note::FellBack {
            from: AccessPath::Ordered,
            to: AccessPath::Scan,
        }],
    );
    // The half that matters more: the note is about cost and the answer is
    // unchanged. An index changes what a read costs and never what it answers.
    assert_eq!(with, without);
}

#[test]
fn the_note_reads_as_a_sentence_and_names_both_paths() {
    // A kind a client can group on, and a message a person can act on. Both,
    // because the two audiences want different things from the same note.
    let note = Note::FellBack {
        from: AccessPath::Ordered,
        to: AccessPath::Scan,
    };
    assert_eq!(note.kind(), "fell-back");
    let message = note.message();
    assert!(message.contains("ordered"), "{message}");
    assert!(message.contains("scan"), "{message}");
}

#[test]
fn an_ordered_read_with_no_index_raises_nothing() {
    // The test this whole design turns on. `ORDER BY … LIMIT` over an unindexed
    // table is the most ordinary read in the language, and an implementation
    // keyed on the planner's shape rather than on an index that declined would
    // raise a note on every single one of them.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 20);

    let (found, notes, path) = answered(
        &mut session,
        "SELECT * FROM events ORDER BY at DESC LIMIT 5;",
    );
    assert_eq!(path, AccessPath::Scan);
    assert_eq!(found.len(), 5);
    assert!(notes.is_empty(), "an unindexed order raised {notes:?}");
}

#[test]
fn an_index_that_served_the_order_raises_nothing() {
    // The other side of the same rule: a note on a read that worked is noise,
    // and noise is how a channel stops being read.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 200);
    session.run(INDEX).unwrap();

    let (found, notes, path) = answered(
        &mut session,
        "SELECT * FROM events ORDER BY at DESC LIMIT 5;",
    );
    assert_eq!(path, AccessPath::Ordered);
    assert_eq!(found.len(), 5);
    assert!(notes.is_empty(), "a served order raised {notes:?}");
}

#[test]
fn a_materialised_source_that_reached_its_ceiling_says_so() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 20);

    let (found, notes, _) = answered(
        &mut session,
        "SELECT * FROM (SELECT * FROM events LIMIT 5);",
    );
    assert_eq!(
        notes,
        vec![Note::SubqueryCeiling { rows: 5 }],
        "twenty records through a ceiling of five",
    );
    // The outer statement answered about the prefix, which is exactly what the
    // note says it did — five records, not twenty.
    assert_eq!(found.len(), 5);
}

#[test]
fn a_ceiling_the_inner_read_never_reached_raises_nothing() {
    // A bound is the caller's own word. Writing one and staying under it is not
    // a truncation and not worth a note; the note exists because a bound that
    // was *reached* and one that was not look identical in the answer.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 3);

    let (found, notes, _) = answered(
        &mut session,
        "SELECT * FROM (SELECT * FROM events LIMIT 50);",
    );
    assert_eq!(found.len(), 3);
    assert!(
        notes.is_empty(),
        "a ceiling nobody reached raised {notes:?}"
    );
}

#[test]
fn the_ceiling_note_survives_the_condition_over_the_source() {
    // The `WHERE` after a materialised read filters records already in hand, so
    // the answer can be shorter than the ceiling while the ceiling was still
    // reached. That is the case where losing the note would hurt most: five
    // records in, two out, and nothing in the answer hinting the inner read had
    // more to give.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 20);

    let (found, notes, _) = answered(
        &mut session,
        "SELECT * FROM (SELECT * FROM events LIMIT 5) WHERE slot = 0;",
    );
    assert!(found.len() < 5, "the condition kept everything: {found:?}");
    assert_eq!(notes, vec![Note::SubqueryCeiling { rows: 5 }]);
}

#[test]
fn an_approximate_answer_says_that_it_is_one() {
    // The one read in this store where an index answers *differently* from a
    // scan rather than faster than one. Without the note an approximate answer
    // and an exact one are the same shape, the same length, and usually the same
    // records — which is what makes the difference worth reporting rather than
    // leaving to whoever remembers writing `APPROXIMATE`.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE items;").unwrap();
    let mut script = String::new();
    for n in 0..40_u32 {
        script.push_str(&format!(
            "CREATE items:{n} = {{ at: [{}.0, {}.0] }};\n",
            n,
            n * 2
        ));
    }
    session.run(&script).unwrap();

    const NEAR: &str = "SELECT * FROM items ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 5";

    // Exact first, with no graph to serve it and therefore nothing to report.
    let (exact, notes, path) = answered(&mut session, &format!("{NEAR} APPROXIMATE;"));
    assert_eq!(path, AccessPath::Scan);
    assert!(notes.is_empty(), "an exact answer raised {notes:?}");

    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();

    let (near, notes, path) = answered(&mut session, &format!("{NEAR} APPROXIMATE;"));
    // The path and the note say the same thing in two registers, and both are
    // kept: the path is the cost word a caller groups by, the note is the
    // sentence a reader is meant to read.
    assert_eq!(path, AccessPath::Approximate);
    assert_eq!(notes, vec![Note::Approximate]);
    assert_eq!(notes[0].kind(), "approximate");
    // Approximate is a statement about the guarantee, not a licence to answer
    // badly: over forty points on a line the walk finds the same five.
    assert_eq!(near, exact);
}

#[test]
fn a_read_that_did_not_ask_for_an_approximation_raises_nothing() {
    // `APPROXIMATE` is the caller's word and the index does not serve a read
    // that did not say it. The path already says so; the note must agree.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE items;").unwrap();
    let mut script = String::new();
    for n in 0..40_u32 {
        script.push_str(&format!("CREATE items:{n} = {{ at: [{n}.0, 0.0] }};\n"));
    }
    session.run(&script).unwrap();
    session
        .run("DEFINE INDEX by_at ON items FIELDS at VECTOR euclidean;")
        .unwrap();

    let (found, notes, path) = answered(
        &mut session,
        "SELECT * FROM items ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 5;",
    );
    assert_eq!(path, AccessPath::Scan);
    assert_eq!(found.len(), 5);
    assert!(notes.is_empty(), "an exact read raised {notes:?}");
}

#[test]
fn an_ordinary_read_carries_no_notes_at_all() {
    // The baseline every other test is measured against, and the property that
    // makes the channel worth having: a note is worth reading because it is
    // rare.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 10);

    for script in [
        "SELECT * FROM events;",
        "SELECT * FROM events WHERE slot = 0;",
        "SELECT * FROM events LIMIT 3;",
        "SELECT count(*) AS n FROM events;",
    ] {
        let (_, notes, _) = answered(&mut session, script);
        assert!(notes.is_empty(), "`{script}` raised {notes:?}");
    }
}

#[test]
fn a_note_from_a_joined_read_reaches_the_answer() {
    // A materialised read on a join side is still a read, and its ceiling is
    // still invisible in the row it produced. The join is where a note is most
    // easily lost, because the records go through a build map on the way out.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people;\n\
             CREATE people:1 = { name: 'ada', slot: 0 };\n\
             CREATE people:2 = { name: 'grace', slot: 0 };\n\
             CREATE people:3 = { name: 'alan', slot: 1 };",
        )
        .unwrap();
    populate(&mut session, 6);

    let (found, notes, _) = answered(
        &mut session,
        "SELECT * FROM events AS e \
         JOIN (SELECT * FROM people LIMIT 2) AS p ON e.slot = p.slot;",
    );
    assert_eq!(notes, vec![Note::SubqueryCeiling { rows: 2 }]);
    assert!(!found.is_empty(), "the join answered nothing");
}

#[test]
fn a_note_never_reaches_an_outcome_that_is_not_records() {
    // `notes()` is defined on every outcome so a caller writes one loop and no
    // branch. What it must never do is invent one.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session, 3);

    for script in [
        "DEFINE TABLE other;",
        "DELETE FROM events WHERE slot = 0 LIMIT ALL;",
    ] {
        let outcomes = session.run(script).unwrap();
        assert!(
            outcomes.last().unwrap().notes().is_empty(),
            "`{script}` carried notes",
        );
    }
}
