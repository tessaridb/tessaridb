//! `MATCHES FUZZY` — the word a reader meant rather than the one they typed.
//!
//! # Two paths, one answer, and here the temptation to break that is strongest
//!
//! Every assertion runs twice: on a table with a search index and on the same
//! data without one. For this operator that is not a formality. The obvious
//! implementation makes the mandatory non-fuzzy prefix an *index-side* bound —
//! walk the dictionary from the first few characters, filter that run by edit
//! distance — and lets the scan compare distance without it. The result compiles,
//! passes every single-path test, and quietly returns a **larger** set on a table
//! with no index than on the same table once somebody declares one.
//!
//! So the prefix is semantics. Both paths apply it, and the assertion that they
//! agree is what holds that in place (ADR-0046).
//!
//! # What the contract refuses, and what it merely declines to serve
//!
//! **A word shorter than the mandatory prefix is a refusal**, raised before any
//! access path is chosen.
//!
//! **The expansion caps are not refusals.** A word whose walk matches too many
//! terms, or reads too many, is answered by the scan instead. Only the index can
//! see either number, so a cap that refused would make a statement succeed
//! without an index and fail once somebody added one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The three words G022's baseline named, plus the distractors that make a wrong
/// answer visible.
///
/// The analyzer stems, deliberately. A misspelling does not stem where its
/// correct spelling does, so an unstemmed fixture would never reach the case this
/// operator is hardest to get right on.
fn peopled(indexed: bool) -> Store {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = { body: 'Vector search over a store' };\n\
             CREATE notes:2 = { body: 'A container for the analyzer' };\n\
             CREATE notes:3 = { body: 'Locking and contention' };\n\
             CREATE notes:4 = { body: 'Running a compaction' };\n\
             CREATE notes:5 = { body: 'The doctor and the victor' };",
        )
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    store
}

/// The ids a read answered with, sorted so two paths compare as sets, and the
/// path it took.
fn answered(session: &mut Session<'_>, read: &str) -> (Vec<String>, AccessPath) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut found: Vec<String> = records.iter().map(|(id, _)| id.to_string()).collect();
    found.sort();
    (found, plan.access)
}

fn opened(indexed: bool) -> (Store, &'static str) {
    (peopled(indexed), "USE NAMESPACE prod; USE DATABASE shop;")
}

/// Run one statement against both fixtures, assert they agree, return what they
/// agreed on.
///
/// The agreement is one assertion and not the whole test. Two paths sharing an
/// analyzer can be wrong in the same way, so every caller then says what the
/// answer should have been.
fn both(statement: &str) -> Vec<String> {
    let (indexed, use_it) = opened(true);
    let (scanned, _) = opened(false);
    let mut with = Session::new(&indexed);
    let mut without = Session::new(&scanned);
    with.run(use_it).unwrap();
    without.run(use_it).unwrap();
    let (left, path) = answered(&mut with, statement);
    let (right, _) = answered(&mut without, statement);
    assert_eq!(
        left, right,
        "the index and the scan disagreed on {statement:?} (indexed path {path:?})"
    );
    left
}

/// The goal's own baseline, which measured zero for all three before this
/// operator existed.
#[test]
fn the_three_words_the_baseline_named_now_find_their_records() {
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES FUZZY 'vectr';"),
        vec!["1".to_owned()],
        "vectr should reach vector"
    );
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES FUZZY 'containr';"),
        vec!["2".to_owned()],
        "containr should reach container"
    );
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES FUZZY 'analyzr';"),
        vec!["2".to_owned()],
        "analyzr should reach analyzer"
    );
}

/// A correctly spelled word still finds itself. Stated because an implementation
/// that only ever compared *misspellings* would pass every test above and fail
/// the query readers actually send most often.
#[test]
fn a_word_spelled_correctly_still_matches() {
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES FUZZY 'vector';"),
        vec!["1".to_owned()]
    );
}

/// The two levels, unchanged from the other two operators: a conjunction across
/// the words typed, a disjunction within each.
#[test]
fn fuzzy_is_still_a_conjunction_across_the_words_typed() {
    assert_eq!(
        both("SELECT id FROM notes WHERE body MATCHES FUZZY 'containr analyzr';"),
        vec!["2".to_owned()]
    );

    // One word nothing comes near empties the conjunction, however well the
    // other matched.
    let none = both("SELECT id FROM notes WHERE body MATCHES FUZZY 'containr zzzqqq';");
    assert!(none.is_empty(), "{none:?}");
}

/// **The stated cost of the mandatory prefix.** A mistake inside the first
/// characters is not found, and this is the case that shows it: `xector` is one
/// edit from `vector` and reaches nothing, while `vectr` is also one edit and
/// reaches it.
///
/// This is the assertion that would fail if somebody later "improved" the
/// operator by dropping the prefix restriction, and it is why the cost is in
/// `docs/tessariql.md` rather than left to be discovered.
#[test]
fn a_mistake_inside_the_mandatory_prefix_is_not_found() {
    let none = both("SELECT id FROM notes WHERE body MATCHES FUZZY 'xector';");
    assert!(none.is_empty(), "xector reached {none:?}");
}

/// The budget is a ceiling that holds. `doctor` and `victor` are in the fixture
/// precisely so that a word three edits away has somewhere plausible to land if
/// the budget leaked.
#[test]
fn a_word_beyond_the_budget_is_not_reached() {
    // `vector` → `victor` is one edit and shares `v`, but not the first three
    // characters, so the prefix rule excludes it before the budget is consulted.
    let found = both("SELECT id FROM notes WHERE body MATCHES FUZZY 'victor';");
    assert_eq!(found, vec!["5".to_owned()], "victor is its own word here");

    // Three edits from any stored term, and inside no term's prefix.
    let none = both("SELECT id FROM notes WHERE body MATCHES FUZZY 'zzzzzz';");
    assert!(none.is_empty(), "{none:?}");
}

/// **A refusal, and it is raised on both paths.** That is the whole point of
/// checking the contract before an access path is chosen: a word too short to
/// have a mandatory prefix fails the same way with an index and without one.
#[test]
fn a_word_shorter_than_the_mandatory_prefix_is_refused_on_both_paths() {
    for indexed in [true, false] {
        let (held, use_it) = opened(indexed);
        let mut session = Session::new(&held);
        session.run(use_it).unwrap();
        let error = session
            .run("SELECT id FROM notes WHERE body MATCHES FUZZY 've';")
            .expect_err("two characters cannot carry a three-character prefix");
        assert!(
            matches!(error, Error::PrefixTooShort { .. }),
            "indexed={indexed} gave {error:?}"
        );
        // The refusal states the limit rather than merely declining.
        assert!(
            error.to_string().contains('3'),
            "the refusal did not state the limit: {error}"
        );
    }
}

/// The index actually serves it, and the table without one actually scans.
///
/// Without this the parity assertions above could all be comparing two scans,
/// which would make the whole file agree with itself and prove nothing about the
/// dictionary walk.
#[test]
fn the_index_serves_it_and_the_scan_answers_without_one() {
    let (indexed, use_it) = opened(true);
    let mut with = Session::new(&indexed);
    with.run(use_it).unwrap();
    let (_, path) = answered(
        &mut with,
        "SELECT id FROM notes WHERE body MATCHES FUZZY 'vectr';",
    );
    assert_eq!(path, AccessPath::Index, "the index did not serve it");

    let (scanned, _) = opened(false);
    let mut without = Session::new(&scanned);
    without.run(use_it).unwrap();
    let (_, path) = answered(
        &mut without,
        "SELECT id FROM notes WHERE body MATCHES FUZZY 'vectr';",
    );
    assert_eq!(path, AccessPath::Scan, "a table with no index used one");
}

/// **G022 S2 — an expanded term can never outrank the word that was actually
/// typed.**
///
/// The corpus the criterion names: a stored misspelling, the intended word, and
/// one record holding both. A fuzzy query for the correct spelling reaches all
/// three, so the ranking is the only thing that separates them.
///
/// # Why this holds, and why saying so is not enough
///
/// `rank::Corpus.asked` is `analyzer.terms(<the query string>)` — the words that
/// were **typed** — and `rank::score` skips any asked term the record does not
/// hold. A record reached only through an expansion therefore holds none of them
/// and scores zero, while a record holding the typed word scores strictly above
/// zero: `idf` is positive under this store's `1 +` deviation and the saturation
/// term is positive for a held term. So the expansion is a zero constant, which
/// is the "constant-score expansion" the criterion allows.
///
/// That is a claim about the code, and S2 says in as many words that a summary
/// verdict is not evidence. It is asserted here instead, on the corpus the
/// criterion describes, so that a later change which gave an expanded term any
/// weight at all would fail rather than merely read differently.
///
/// Indexed only, and not an omission: `search::score` refuses without an index
/// rather than inventing collection statistics, so there is no scan-side ranking
/// for a parity assertion to compare against.
#[test]
fn an_expanded_term_never_outranks_the_word_that_was_typed() {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = { body: 'vector' };\n\
             CREATE notes:2 = { body: 'vectr' };\n\
             CREATE notes:3 = { body: 'vectr vector' };\n\
             CREATE notes:4 = { body: 'compaction' };\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
        )
        .unwrap();

    let outcomes = session
        .run(
            "SELECT id, search::score(body, 'vector') AS relevance \
             FROM notes WHERE body MATCHES FUZZY 'vector';",
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };

    let scored: Vec<(String, f64)> = records
        .iter()
        .map(|(id, held)| {
            let tessari_types::Value::Object(fields) = held else {
                panic!("a record projected as {held:?}");
            };
            let relevance = match fields.get("relevance") {
                Some(tessari_types::Value::Number(number)) => number
                    .as_float()
                    .unwrap_or_else(|| panic!("relevance was not a float: {number:?}")),
                other => panic!("relevance came back as {other:?}"),
            };
            (id.to_string(), relevance)
        })
        .collect();

    // The expansion reached the misspelling: without this the test would pass on
    // an engine that simply never matched it, which is the vacuous shape.
    assert!(
        scored.iter().any(|(id, _)| id.ends_with('2')),
        "the expansion did not reach the stored misspelling: {scored:?}",
    );
    // `notes:1` holds only the typed word, `notes:2` only the misspelling,
    // `notes:3` both — the record the criterion names.
    let relevance = |which: char| {
        scored
            .iter()
            .find(|(id, _)| id.ends_with(which))
            .unwrap_or_else(|| panic!("notes:{which} was not answered: {scored:?}"))
            .1
    };

    // A record reached only through the expansion scores nothing…
    assert!(
        relevance('2').abs() < f64::EPSILON,
        "an expanded term carried weight: {scored:?}",
    );
    // …and every record holding the word that was actually typed outranks it.
    for holder in ['1', '3'] {
        assert!(
            relevance(holder) > relevance('2'),
            "notes:{holder} did not outrank the misspelling: {scored:?}",
        );
    }
}
