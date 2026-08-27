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
//! Those floors are **global**, which is their own limit: a single configuration
//! that changes nothing is invisible inside an aggregate the others satisfy.
//! Measured rather than argued — an inert configuration was injected and the
//! floors passed. So there is a third half: each configuration's vector of
//! reported plans must differ from every other configuration's, and each of the
//! three plans this goal made reachable must actually be reached, named one at a
//! time. Those are on the **plan**, not the access name, because the access name
//! is lossy by measurement: over the eight original configurations, `by_last`,
//! `by_name`, `by_name UNIQUE` and both-together produced a byte-identical
//! vector of access names across all forty reads.
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
//! - removing the fixed-column ranking rule → **passed**, and correctly, while
//!   this file compared answers alone. So did reverting to offering only the
//!   first matching index. Both are **cost** regressions, and an *answer* matrix
//!   cannot see one by construction: the answer is identical whichever index
//!   narrowed.
//!
//!   That boundary moved once the file began comparing the reported **plans**
//!   as well. Reverting the ranking that puts a candidate's narrowing proof
//!   above the shape heuristic now fails here — the range-under-an-equality
//!   plan disappears and the assertion naming it fires — while every answer
//!   assertion stays green, which is precisely the shape of a cost regression.
//!   The counted assertions in `full_tuple.rs` and `composite_range.rs` remain
//!   the ones that measure *how much*; this file catches only that the plan
//!   changed. Both boundaries are worth stating: a file that claimed to cover
//!   cost would be the coverage illusion this one exists to refuse.
//! - returning the ordered path's records reversed → **passes**, and that is a
//!   fact about the store rather than a hole here: the read sorts whatever the
//!   source hands it, so a source returning the right records in the wrong order
//!   cannot change an answer. What the ordered path saves is reading the table,
//!   not running the sort (**Q-74**).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

/// How many records every configuration holds.
///
/// Enough that a scan and a bounded index read are different amounts of work,
/// and that a surname group is larger than any bound asked for here.
const RECORDS: i64 = 240;

/// How many surnames the records share between them.
const SURNAMES: i64 = 8;

/// The schema each configuration declares, each a fresh store over the same
/// records.
///
/// Statements, not only indexes: a declaration can decide which plans are
/// reachable as surely as an index can, and configuration 8 is the case —
/// an ascending order is served only over a field declared `REQUIRED`, so
/// without the declaration the index is present and the plan is not.
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
    // The ascending order, reachable only here. Every record holds `at`, so the
    // declaration is accepted against the table that already exists — measured
    // in `ascending_order.rs`, where it is refused against one that breaks it.
    // The data is identical to configuration 4's; the word `REQUIRED` is the
    // whole difference between a plan and no plan.
    &[
        "DEFINE FIELD at ON people TYPE int REQUIRED;",
        "DEFINE INDEX by_at ON people FIELDS at;",
    ],
    // A composite leading with `at` and **no** single-field rival, so the order
    // on `at` has nowhere else to come from. Every other configuration that
    // serves an order serves it from `by_at`.
    &["DEFINE INDEX by_at_last ON people FIELDS at, last;"],
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
    // A range on a composite's second field under an equality on its first.
    "last = 'l3' AND first > 'f1' AND first < 'f9'",
    // The same conjuncts written range-first. Gathering must not depend on which
    // the author wrote first — and this is the reverse of the line above rather
    // than of some older shape, so it tests that property on the shape that
    // introduced it.
    "first > 'f1' AND first < 'f9' AND last = 'l3'",
];

/// The order and window each condition is asked with.
const SHAPES: &[&str] = &[
    "",
    " ORDER BY at DESC LIMIT 10",
    " ORDER BY at DESC START 3 LIMIT 5",
    // Ascending. Refused in every configuration but 8, and the reason is a fact
    // about the *declaration* and not about the data: an absent value sorts
    // before every present one and the index holds no entry for it, so the walk
    // would answer short — but no record here lacks `at`, so what makes the read
    // unsound in configurations 4, 6 and 7 is only that nothing forbids such a
    // record from being written next. Configuration 8 declares `at` `REQUIRED`
    // and the same read is served.
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
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        plan.access,
    )
}

/// The whole plan `EXPLAIN` reports, flattened into one comparable word.
///
/// Every field, not only `access` — because the access name alone is lossy in a
/// way that was measured rather than guessed: over the eight original
/// configurations, `by_last`, `by_name`, `by_name UNIQUE` and both-together
/// produce a **byte-identical** vector of access names across all forty reads.
/// Four of seven configurations were indistinguishable to a matrix comparing
/// that. The index name and the shape are what tell them apart.
fn planned(session: &mut Session<'_>, read: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    let mut parts = Vec::new();
    for key in ["access", "index", "shape", "columns", "at_most"] {
        if let Some(held) = fields.get(key) {
            let shown = match held {
                Value::String(text) => text.clone(),
                other => format!("{other:?}"),
            };
            parts.push(format!("{key}={shown}"));
        }
    }
    parts.join(" ")
}

/// The access name alone, for the one assertion that is about it.
fn access(plan: &str) -> &str {
    plan.strip_prefix("access=")
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or("")
}

/// Every read the matrix runs.
///
/// The cross-product, and then the same shapes with **no condition at all** —
/// which is not symmetry for its own sake. The ordered walk under a condition is
/// descending-only by construction and says so twice (`descend_matching` calls
/// it "the second lock on the same door"), so an ascending order can only ever
/// be served from an index when nothing is narrowing the read. Measured before
/// it was believed: with `at` declared `REQUIRED` and indexed, every one of the
/// twelve conditioned ascending cells still reports `scan` or a plain index
/// range. Without this second family the ascending plan is absent from the
/// matrix for a reason that has nothing to do with any configuration in it.
fn reads() -> Vec<String> {
    let mut found = Vec::new();
    for condition in CONDITIONS {
        for shape in SHAPES {
            found.push(format!("SELECT * FROM people WHERE {condition}{shape};"));
        }
    }
    for shape in SHAPES {
        found.push(format!("SELECT * FROM people{shape};"));
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
    // One entry per configuration: the plans it reported, in read order. What an
    // inert configuration is caught by.
    let mut vectors: Vec<(usize, Vec<String>)> = Vec::new();
    // The three plans this goal made reachable, each true only once a cell
    // actually reports it.
    let mut ascending_from_an_index = false;
    let mut an_order_from_a_composite = false;
    let mut a_range_under_an_equality = false;
    for (n, indexes) in CONFIGURATIONS.iter().enumerate().skip(1) {
        let held = store();
        let mut session = ready(&held, indexes);
        let mut reported_here = Vec::with_capacity(reads.len());
        for (read, expected) in reads.iter().zip(&answers) {
            let (found, path) = run(&mut session, read);
            // The whole point, and in **answer order**: a matrix that sorted the
            // answers could not see the one thing three of these waves changed.
            assert_eq!(&found, expected, "{indexes:?} :: {read}");
            // The dangerous direction only. A plan reporting `scan` for a read
            // an index served would hide the index; a plan naming a path the
            // read tried and could not fill is the documented imprecision.
            let plan = planned(&mut session, read);
            let reported = access(&plan);
            assert!(
                reported != "scan" || path == AccessPath::Scan,
                "the plan under-promised :: {indexes:?} :: {read}"
            );
            if reported != path.name() {
                optimistic = optimistic.saturating_add(1);
            }
            if read.contains("ORDER BY at LIMIT") && path == AccessPath::Ordered {
                ascending_from_an_index = true;
            }
            if reported == "ordered" && plan.contains("index=by_at_last") {
                an_order_from_a_composite = true;
            }
            if plan.contains("shape=range") && plan.contains("columns=Number(Integer(2))") {
                a_range_under_an_equality = true;
            }
            seen.insert(path.name());
            if path != AccessPath::Scan {
                varied = varied.saturating_add(1);
            }
            reported_here.push(plan);
        }
        vectors.push((n, reported_here));
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

    // The three plans this goal made reachable, asserted one at a time. A
    // summary count would let two of them stand in for the third.
    //
    // Before the per-configuration check below, deliberately. The two overlap:
    // configuration 8's *only* distinguishing feature is the ascending plan, so
    // anything that takes the plan away also makes its vector identical to
    // configuration 4's. Whichever assertion runs first is the one whose message
    // the reader gets, and "no ascending read was served from an index" says
    // what happened where "configurations 4 and 8 are identical" only says that
    // something did.
    assert!(
        ascending_from_an_index,
        "no ascending read was served from an index — configuration 8 declares \
         `at` REQUIRED precisely so one is"
    );
    assert!(
        an_order_from_a_composite,
        "no order was served from a composite index — configuration 9 has no \
         single-field rival precisely so one is"
    );
    assert!(
        a_range_under_an_equality,
        "no range under an equality was served from the composite holding both"
    );

    // No configuration is inert. The floor above is a global count, and one
    // configuration that changes nothing is invisible inside an aggregate the
    // other configurations satisfy — so it is asserted per configuration
    // instead, against every other one rather than against the oracle alone.
    //
    // On the plan and not the access name, because the access name is lossy by
    // measurement: over the eight original configurations, four of the seven
    // non-oracle ones produced a byte-identical vector of access names.
    for (n, held) in &vectors {
        for (other, against) in &vectors {
            assert!(
                n == other || held != against,
                "configurations {n} and {other} report identical plans for all \
                 {} reads — one of them reaches nothing the other does not",
                reads.len()
            );
        }
    }
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

    // The range under the equality, derived from the fixture's own two rules
    // rather than from anything the store did: `last = 'l3'` is `n % 8 == 3`,
    // and the bound is a **string** comparison, so `f91` and `f99` fall outside
    // `'f1' < x < 'f9'` while `f107` sits comfortably inside it. A range that
    // compared numbers would answer differently, and it would answer
    // differently in a way the whole cross-product could agree on.
    let expected: Vec<RecordId> = (1..=RECORDS)
        .filter(|n| n % SURNAMES == 3)
        .filter(|n| {
            let first = format!("f{n}");
            first.as_str() > "f1" && first.as_str() < "f9"
        })
        .rev()
        .map(RecordId::Int)
        .collect();
    let (ranged, path) = run(
        &mut session,
        "SELECT * FROM people WHERE last = 'l3' AND first > 'f1' AND first < 'f9' \
         ORDER BY at DESC;",
    );
    assert_eq!(ranged, expected);
    assert!(!expected.contains(&RecordId::Int(91)));
    assert_eq!(path, AccessPath::Index);
}
