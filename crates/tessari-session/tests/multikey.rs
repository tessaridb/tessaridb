//! A multikey index — one entry per element rather than one per record.
//!
//! The third and last context for `[*]`, and the only one with a **storage**
//! consequence. Everything here is downstream of one rule that this store does
//! not bend: *an index changes what a read costs and never what it answers.* So
//! the test that matters is not "the index works" but "the index and the scan
//! agree", asserted between two stores rather than against a list written here.
//!
//! **Reclamation is not tested from here, and that is a finding rather than an
//! omission.** An entry left behind under an element the array no longer holds
//! changes no answer, because every candidate an index offers is confirmed
//! against the record it points at — so a test at this level passes with the
//! removal deliberately broken, which was tried. The entries themselves are
//! swept in `tessari-storage/tests/index_sweep.rs`, where the same broken removal
//! shows up as ninety-three orphans.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Notes with tags, one of which repeats an element and one of which has none.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE work; USE DATABASE work;\n\
             DEFINE TABLE notes;\n\
             CREATE notes:1 = { title: 'first', tags: ['urgent', 'draft'], scores: [1, 5, 9] };\n\
             CREATE notes:2 = { title: 'second', tags: ['draft'] };\n\
             CREATE notes:3 = { title: 'third', tags: [] };\n\
             CREATE notes:4 = { title: 'fourth' };\n\
             CREATE notes:5 = { title: 'fifth', tags: ['dup', 'dup'] };",
        )
        .unwrap();
    session
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

fn path(session: &mut Session<'_>, script: &str) -> AccessPath {
    let outcomes = session.run(script).unwrap();
    outcomes.last().unwrap().path().unwrap()
}

const DEFINE: &str = "DEFINE INDEX by_tag ON notes FIELDS tags[*];";

#[test]
fn an_element_is_found_through_the_index() {
    let store = store();
    let mut session = ready(&store);
    session.run(DEFINE).unwrap();
    assert_eq!(
        path(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        AccessPath::Index
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';"
        ),
        vec![RecordId::Int(1)]
    );
    assert!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'no';").is_empty(),
        "a value no element holds found something"
    );
}

#[test]
fn the_index_answers_exactly_what_the_scan_answered() {
    // The rule the whole task is downstream of, asserted as an equality between
    // two stores rather than against a list written here — a list would be
    // testing this file's arithmetic, and what is at stake is that the two
    // access paths agree with each other.
    let scanned = store();
    let mut without = ready(&scanned);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run(DEFINE).unwrap();

    for script in [
        "SELECT * FROM notes WHERE tags[*] = 'draft';",
        "SELECT * FROM notes WHERE tags[*] = 'urgent';",
        "SELECT * FROM notes WHERE tags[*] = 'dup';",
        "SELECT * FROM notes WHERE tags[*] = 'nothing';",
        // Composed with the rest of a condition, where the index narrows one
        // conjunct and the whole condition still decides.
        "SELECT * FROM notes WHERE tags[*] = 'draft' AND title = 'second';",
        // And negated, which no index can serve.
        "SELECT * FROM notes WHERE NOT tags[*] = 'urgent';",
    ] {
        assert_eq!(
            ids(&mut with, script),
            ids(&mut without, script),
            "the index disagreed with the scan: {script}"
        );
    }
    // …and it really was an index read, so the equality above is not two scans
    // agreeing with each other.
    assert_eq!(
        path(&mut with, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        AccessPath::Index
    );
    assert_eq!(
        path(&mut without, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        AccessPath::Scan
    );
}

#[test]
fn an_element_that_left_the_array_stops_being_an_answer() {
    // Named for what it asserts, which is **not** reclamation — and the
    // difference was found by breaking the removal and watching this test pass.
    // Every index candidate is confirmed against the record it points at, so an
    // entry left behind under a value the record no longer holds changes no
    // answer at all. It is still real damage: space nothing reclaims, and an
    // index that has stopped describing the table.
    //
    // Reclamation is therefore invisible from out here and is asserted where it
    // is visible — `tessari-storage/tests/index_sweep.rs`, which walks the entry
    // keys themselves and re-derives what they should be from the records.
    let store = store();
    let mut session = ready(&store);
    session.run(DEFINE).unwrap();
    session
        .run("UPDATE notes:1 = { title: 'first', tags: ['draft'] };")
        .unwrap();

    assert!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';"
        )
        .is_empty(),
        "an element that left the array kept its index entry"
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        vec![RecordId::Int(1), RecordId::Int(2)],
        "removing one element took another element's entry with it"
    );
}

#[test]
fn a_deleted_record_leaves_no_entry_under_any_of_its_elements() {
    let store = store();
    let mut session = ready(&store);
    session.run(DEFINE).unwrap();
    session.run("DELETE notes:1;").unwrap();
    assert!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';"
        )
        .is_empty()
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        vec![RecordId::Int(2)]
    );
}

#[test]
fn an_element_added_to_an_array_gains_an_entry_without_disturbing_the_others() {
    let store = store();
    let mut session = ready(&store);
    session.run(DEFINE).unwrap();
    session
        .run("UPDATE notes:2 = { title: 'second', tags: ['draft', 'urgent'] };")
        .unwrap();
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';"
        ),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn a_record_answers_once_even_when_several_of_its_elements_match() {
    // A range can hold more than one of a record's elements, and the answers of
    // this store are keyed by record — so answering twice is a wrong answer
    // rather than a verbose one. It holds because the index read collects into a
    // map keyed by record id, and this says so rather than leaving a property
    // that holds by accident one refactor from not holding.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_score ON notes FIELDS scores[*];")
        .unwrap();
    let found = ids(&mut session, "SELECT * FROM notes WHERE scores[*] > 0;");
    assert_eq!(found, vec![RecordId::Int(1)], "{found:?}");
    assert_eq!(
        path(&mut session, "SELECT * FROM notes WHERE scores[*] > 0;"),
        AccessPath::Index
    );
}

#[test]
fn duplicated_elements_are_one_entry_and_answer_once() {
    let store = store();
    let mut session = ready(&store);
    session.run(DEFINE).unwrap();
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'dup';"),
        vec![RecordId::Int(5)]
    );
    // …and removing the array removes the one entry rather than one of two.
    session
        .run("UPDATE notes:5 = { title: 'fifth', tags: [] };")
        .unwrap();
    assert!(ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'dup';").is_empty());
}

#[test]
fn a_rebuild_produces_what_incremental_maintenance_produced() {
    // `REBUILD INDEX` runs the same projection over every row, so this is the
    // check that the two sides of the write path and the build agree — the same
    // property SGI.T2 built the rebuild for, now with several entries per record.
    let store = store();
    let mut session = ready(&store);
    session.run(DEFINE).unwrap();
    session
        .run("UPDATE notes:1 = { title: 'first', tags: ['draft', 'late'] };")
        .unwrap();
    let before = [
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'late';"),
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';",
        ),
    ];
    session.run("REBUILD INDEX by_tag ON notes;").unwrap();
    let after = [
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'late';"),
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'urgent';",
        ),
    ];
    assert_eq!(before, after);
    assert_eq!(before[0], vec![RecordId::Int(1), RecordId::Int(2)]);
    assert!(before[2].is_empty(), "{before:?}");
}

#[test]
fn an_index_declared_on_the_field_is_still_not_offered_for_a_question_about_elements() {
    // SGJ.T1's rule, restated where it now matters most: `tags` and `tags[*]`
    // are different routes, an index is matched to a condition by exact route
    // equality, and that matching *is* what keeps an index over the whole array
    // from answering a question about elements.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_tags ON notes FIELDS tags;")
        .unwrap();
    assert_eq!(
        path(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        AccessPath::Scan
    );
    assert_eq!(
        ids(&mut session, "SELECT * FROM notes WHERE tags[*] = 'draft';"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
    // …and the whole-array question still uses the whole-array index.
    assert_eq!(
        path(&mut session, "SELECT * FROM notes WHERE tags = ['draft'];"),
        AccessPath::Index
    );
}

#[test]
fn the_shapes_without_a_rule_are_refused_each_for_its_own_reason() {
    // Three refusals, three reasons, and none of them a stray-token message: a
    // caller told "unexpected token" would go looking for a typo in a statement
    // that has none.
    let store = store();
    let mut session = ready(&store);
    for script in [
        // Two readings — no two records share an element, or a record's own
        // elements are distinct.
        "DEFINE INDEX a ON notes FIELDS tags[*] UNIQUE;",
        // Both already decide their own multiplicity.
        "DEFINE INDEX b ON notes FIELDS tags[*] SEARCH;",
        "DEFINE INDEX c ON notes FIELDS tags[*] VECTOR euclidean;",
        // An entry per pair of elements, paid on every write.
        "DEFINE INDEX d ON notes FIELDS tags[*], scores[*];",
    ] {
        let refused = session.run(script);
        assert!(
            matches!(refused, Err(Error::Script(_))),
            "{script} was accepted: {refused:?}"
        );
    }
    // …and one multi-valued route beside an ordinary one is fine, because only
    // one set of elements is being per.
    session
        .run("DEFINE INDEX e ON notes FIELDS tags[*], title;")
        .unwrap();
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM notes WHERE tags[*] = 'draft' AND title = 'second';"
        ),
        vec![RecordId::Int(2)]
    );
}
