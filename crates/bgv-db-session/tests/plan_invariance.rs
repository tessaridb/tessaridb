//! Every plan the planner restructurings made reachable, answering the same
//! thing.
//!
//! Three waves changed which plan a read takes: a condition is now offered to
//! *every* index whose leading field it names rather than the first declared;
//! an equality gathers the leading run of an index's fields rather than one of
//! them; a candidate fixing more columns outranks one fixing fewer; and a
//! bounded descending order is taken from the index even under a `WHERE`.
//!
//! Each of those asserted invariance over the shapes **it** introduced. This
//! file asserts it over the cross-product, which is where a planner regression
//! actually lives: an index configuration nobody wrote a test for, meeting a
//! condition shape from another wave.
//!
//! # What this file must not become, and the reason it is written this way
//!
//! G003 reached 40 of 40 nodes with one criterion false, and the reason was a
//! broad matrix that passed straight through a falsification: **every cell
//! scanned**, so the equality it asserted was several scans agreeing with each
//! other. A matrix that cannot tell "the plans agree" from "there were no plans"
//! is worse than no matrix, because it reads as coverage.
//!
//! So the equality is only half of it. The other half is counted: how many reads
//! were served by **different access paths** in different configurations, and
//! how many distinct paths appeared at all. Both are asserted against floors the
//! file states, so a degenerate matrix fails instead of passing quietly.
//!
//! # And what the matrix found about `EXPLAIN`
//!
//! The first version of this file asserted that `EXPLAIN`'s access and the path
//! the read took agree in every cell. They do not, and the disagreement is
//! **pre-existing and deliberate**: a plan reports the path the planner *chose*,
//! while whether a walk fills its bound is a question only the read can answer.
//! So a read whose condition is too thin for the order gives it up and takes
//! another plan, while its plan still says `ordered`.
//!
//! What is asserted instead is the direction that would be dangerous: `EXPLAIN`
//! **never** reports `scan` for a read an index or an order actually served. A
//! plan that under-promises hides a real index read, and nothing in the store
//! would ever mention it.
//!
//! The other direction is the imprecision, and the matrix showed it takes two
//! forms rather than one: a read that gives the order up falls back to whichever
//! plan is next best, which may be a scan **or** an index — so the plan can say
//! `ordered` while the read says either. Those cells are counted rather than
//! waved through, and **Q-73** carries the question of whether a plan that
//! describes a read nobody ran is one this store should print.

//! # What the falsification pass established, including what this cannot catch
//!
//! Five injections, and the three that did **not** fail are worth more than the
//! two that did:
//!
//! - emptying every configuration → **fails**, on the counted floor. The floor
//!   works, which is the one thing this file most needed to prove about itself.
//! - dropping the re-test of candidates against the whole condition → **fails**.
//!   Wrong answers are caught.
//! - removing the fixed-column ranking rule → **passes**, and correctly. So does
//!   reverting to offering only the first matching index. Both are **cost**
//!   regressions, and an answer matrix cannot see one by construction: the
//!   answer is identical whichever index narrowed. They are caught by the
//!   counted assertions in `full_tuple.rs`, which fail on exactly these
//!   injections. Stating the boundary is the point — a file that claimed to
//!   cover them would be the coverage illusion this one exists to refuse.
//! - returning the ordered path's records reversed → **passes**, and that is a
//!   fact about the store rather than a hole here: the read sorts whatever the
//!   source hands it, so a source returning the right records in the wrong order
//!   cannot change an answer. What the ordered path saves is reading the table,
//!   not running the sort (**Q-74**).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{AccessPath, Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

/// How many records every configuration holds.
///
/// Enough that a scan and a bounded index read are different amounts of work,
/// and that a surname group is larger than any bound asked for here.
const RECORDS: i64 = 240;

/// How many surnames the records share between them.
const SURNAMES: i64 = 8;

/// The index configurations, each a fresh store over the same records.
///
/// Configuration 0 is the oracle. Every other one exists to reach a plan the
/// restructurings made possible, and 5 is the one that could not be reached at
/// all before them: two indexes on one leading field, with the **weaker**
/// declared first, so taking the first match would take the wrong one.
const CONFIGURATIONS: &[&[&str]] = &[
    &[],
    &["DEFINE INDEX by_last ON people FIELDS last;"],
    &["DEFINE INDEX by_name ON people FIELDS last, first;"],
    &["DEFINE INDEX by_name ON people FIELDS last, first UNIQUE;"],
    &["DEFINE INDEX by_at ON people FIELDS at;"],
    &[
        "DEFINE INDEX by_last ON people FIELDS last;",
        "DEFINE INDEX by_name ON people FIELDS last, first;",
    ],
    &[
        "DEFINE INDEX by_at ON people FIELDS at;",
        "DEFINE INDEX by_last ON people FIELDS last;",
    ],
    &[
        "DEFINE INDEX by_last ON people FIELDS last;",
        "DEFINE INDEX by_name ON people FIELDS last, first UNIQUE;",
        "DEFINE INDEX by_at ON people FIELDS at;",
        "DEFINE INDEX by_city ON people FIELDS city;",
    ],
];

/// The conditions, one per shape the planner decides differently about.
const CONDITIONS: &[&str] = &[
    "last = 'l3'",
    "last = 'l3' AND first = 'f11'",
    // The same tuple with the conjuncts reversed: the gathering must not depend
    // on which one the author wrote first, while an equal-ranked *tie* still
    // does.
    "first = 'f11' AND last = 'l3'",
    "city = 'c2'",
    "last = 'l3' AND city = 'c2'",
    "at > 100",
    "at >= 50 AND at < 80",
    "last LIKE 'l%'",
    "slot = 0",
    "last = 'nobody'",
];

/// The order and window each condition is asked with.
const SHAPES: &[&str] = &[
    "",
    " ORDER BY at DESC LIMIT 10",
    " ORDER BY at DESC START 3 LIMIT 5",
    // Ascending: the records with no value sort first and the index does not
    // hold them, so this must never be served from one.
    " ORDER BY at LIMIT 10",
];

/// `RECORDS` people, sharing `SURNAMES` surnames, each with a unique forename.
///
/// `(last, first)` is unique across the table, so the `UNIQUE` composite
/// configuration is definable and its ceiling of one is real. `at` carries ties
/// in pairs, so a tie group straddles every bound asked for here. `slot` is one
/// in eight and no index holds it — the condition an index cannot narrow.
fn ready<'a>(store: &'a Store, indexes: &[&str]) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE people;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 1..=RECORDS {
        script.push_str(&format!(
            "CREATE people:{n} = {{ last: 'l{}', first: 'f{n}', city: 'c{}', \
             at: {}, slot: {} }};\n",
            n % SURNAMES,
            n % 5,
            n / 2,
            n % 8
        ));
    }
    session.run(&script).unwrap();
    // After the records, so every configuration indexes the same data — a
    // build and a maintained index must agree, which `REBUILD INDEX` asserts
    // elsewhere and this file therefore need not.
    for statement in indexes {
        session.run(statement).unwrap();
    }
    session
}

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// What a read answered and how it was served, in answer order.
fn run(session: &mut Session<'_>, read: &str) -> (Vec<RecordId>, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, path }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (records.iter().map(|(id, _)| id.clone()).collect(), *path)
}

/// The access `EXPLAIN` reports for the same read.
fn explained(session: &mut Session<'_>, read: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    match fields.get("access") {
        Some(Value::String(held)) => held.clone(),
        other => panic!("a plan reported access {other:?}"),
    }
}

/// Every read the matrix runs.
fn reads() -> Vec<String> {
    let mut found = Vec::new();
    for condition in CONDITIONS {
        for shape in SHAPES {
            found.push(format!("SELECT * FROM people WHERE {condition}{shape};"));
        }
    }
    found
}

#[test]
fn every_configuration_answers_what_the_scan_answers_and_they_do_not_all_scan() {
    let reads = reads();
    let oracle_store = store();
    let mut oracle = ready(&oracle_store, CONFIGURATIONS[0]);

    // Collected rather than asserted cell by cell, so the *shape* of the matrix
    // can be asserted afterwards — which is the half that catches a matrix where
    // nothing was planned.
    let mut answers: Vec<Vec<RecordId>> = Vec::with_capacity(reads.len());
    for read in &reads {
        let (found, path) = run(&mut oracle, read);
        assert_eq!(path, AccessPath::Scan, "the oracle is a scan: {read}");
        answers.push(found);
    }

    let mut varied = 0_usize;
    // Cells where the plan named a path the read tried and could not fill. Not
    // a failure — see the header — but counted, so it cannot grow silently.
    let mut optimistic = 0_usize;
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for indexes in CONFIGURATIONS.iter().skip(1) {
        let held = store();
        let mut session = ready(&held, indexes);
        for (read, expected) in reads.iter().zip(&answers) {
            let (found, path) = run(&mut session, read);
            // The whole point, and in **answer order**: a matrix that sorted the
            // answers could not see the one thing three of these waves changed.
            assert_eq!(&found, expected, "{indexes:?} :: {read}");
            // The dangerous direction only. A plan reporting `scan` for a read
            // an index served would hide the index; a plan naming a path the
            // read tried and could not fill is the documented imprecision.
            let reported = explained(&mut session, read);
            assert!(
                reported != "scan" || path == AccessPath::Scan,
                "the plan under-promised :: {indexes:?} :: {read}"
            );
            if reported != path.name() {
                optimistic = optimistic.saturating_add(1);
            }
            seen.insert(path.name());
            if path != AccessPath::Scan {
                varied = varied.saturating_add(1);
            }
        }
    }

    // The half with teeth. If every cell scanned, the equality above would hold
    // for a store with no planner at all — which is exactly how G003 came to
    // report a false criterion as passing.
    let cells = reads
        .len()
        .saturating_mul(CONFIGURATIONS.len().saturating_sub(1));
    assert!(
        varied.saturating_mul(4) > cells,
        "only {varied} of {cells} cells were served by an index or an order — \
         a matrix this flat is several scans agreeing with each other"
    );
    assert!(
        seen.contains("index") && seen.contains("ordered") && seen.contains("scan"),
        "the matrix reached only {seen:?}"
    );
    // The imprecision is real and bounded. If it ever reached most of the
    // matrix, `EXPLAIN` would be describing a store nobody is running.
    assert!(
        optimistic.saturating_mul(4) < cells,
        "{optimistic} of {cells} plans named a path the read could not take \
         (Q-73)"
    );
}

#[test]
fn the_matrix_is_not_agreeing_on_a_wrong_answer() {
    // One answer written out by hand, so the whole cross-product cannot be
    // consistently wrong. `at` is `n / 2`, so the highest values belong to the
    // highest identities; `last = 'l3'` holds for `n % 8 == 3`.
    let held = store();
    let mut session = ready(&held, CONFIGURATIONS[7]);
    let (found, path) = run(
        &mut session,
        "SELECT * FROM people WHERE last = 'l3' ORDER BY at DESC LIMIT 3;",
    );
    assert_eq!(
        found,
        vec![RecordId::Int(235), RecordId::Int(227), RecordId::Int(219)]
    );
    assert_eq!(path, AccessPath::Ordered);

    // One record matches, so the order can never be filled from `by_at` however
    // far it walks — and the read does not fall back to a scan, it falls back to
    // the *next best plan*, which is the unique tuple lookup. Both restructurings
    // in one answer: the order gives up, and the condition is served as one
    // lookup rather than through `last` with `first` re-tested.
    let (one, path) = run(
        &mut session,
        "SELECT * FROM people WHERE last = 'l3' AND first = 'f11' ORDER BY at DESC LIMIT 3;",
    );
    assert_eq!(one, vec![RecordId::Int(11)]);
    assert_eq!(path, AccessPath::Index);
}
