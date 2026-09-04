//! "Did you mean" as a field beside the records, never as a substitution into
//! the query.
//!
//! # What these tests are actually guarding
//!
//! Not that a suggestion appears. A suggestion appearing is the easy half and
//! the half a reader notices; the half that fails silently is what happens to
//! the *records* while it appears. A store that helpfully re-ran the query with
//! the corrected term would return a plausible answer to a question nobody
//! asked, and no assertion about the suggestion's content would catch it.
//!
//! Worse, this store already knows exactly how that failure would look. Giving
//! an unheld term any weight in BM25 does not tie the ranking, it inverts it:
//! length normalisation promotes the shortest document, so a substituted term
//! puts the wrong record first wearing an entirely plausible score. So the
//! deciding test here is a **pair** — the records a misspelled query returns are
//! the records it returned before any of this existed, *and* a suggestion sits
//! beside them.
//!
//! # The three states, and why two of them look like nothing
//!
//! A suggestion needs a term dictionary, and only a `SEARCH` index has one. So
//! there are three answers, not two, and the two that carry no correction are
//! not the same answer:
//!
//! - no index — nothing was looked for, and the store says so by saying nothing
//! - an index, every term held — a dictionary was asked and found nothing to fix
//! - an index, a term unheld — the correction
//!
//! Collapsing the first two would let a read over an unindexed field report a
//! confident "nothing is near" that nobody ever checked. That is the assertion
//! `a_field_with_no_index_is_not_a_field_with_nothing_near` exists for, and it
//! is the one a future refactor is most likely to break, because both states
//! print as an empty console line.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session, Suggestion};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// Records chosen so that `vector` is held often, `vecter` is held by nothing,
/// and `engine` is there to be the term a mixed query gets right.
///
/// TWO terms sit one edit from `vecter` — `vector`, held by three records, and
/// `vectar`, held by one. That is deliberate and it is what makes the ranking
/// testable: with a single candidate every ordering rule passes, so a fixture
/// with one near term would assert nothing about which of two the store offers.
fn notes(indexed: bool) -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ body: 'the vector index walks a graph' }};\n\
             CREATE notes:2 = {{ body: 'a vector is a list of numbers' }};\n\
             CREATE notes:3 = {{ body: 'the engine stores every vector' }};\n\
             CREATE notes:4 = {{ body: 'the engine has nothing else in it' }};\n\
             CREATE notes:5 = {{ body: 'vectar is a brand and appears once' }};",
        ))
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    held
}

/// What one read answered with: its record ids, and its suggestion.
type Answer = (Vec<String>, Option<Suggestion>);

/// The ids and the suggestion one read answered with.
fn answered(session: &mut Session<'_>, read: &str) -> Answer {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records {
        records,
        suggestion,
        ..
    }) = outcomes.last()
    else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    let mut found: Vec<String> = records.iter().map(|(id, _)| id.to_string()).collect();
    found.sort();
    (found, suggestion.clone())
}

/// One read on a store with the index and on one without it.
fn both_paths(read: &str) -> (Answer, Answer) {
    let mut answers = Vec::new();
    for indexed in [false, true] {
        let held = notes(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();
        answers.push(answered(&mut session, read));
    }
    (answers[0].clone(), answers[1].clone())
}

/// **The test that decides the wave.** A misspelled query returns the records it
/// would have returned with no suggestion machinery at all, *and* a suggestion.
///
/// The first assertion is the whole of the criterion: no query is ever silently
/// answered as a different query. If a suggestion ever reached the executed
/// query, `vecter` would answer with the three records holding `vector` — a
/// plausible answer, and the wrong one.
#[test]
fn a_misspelled_term_is_suggested_and_never_substituted() {
    let (_, (records, suggestion)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'vecter' ORDER BY id;");

    assert!(
        records.is_empty(),
        "the suggestion reached the executed query and answered a different one: {records:?}"
    );
    let Some(Suggestion::DidYouMean(corrections)) = suggestion else {
        panic!("a term nothing holds earned no suggestion: {suggestion:?}");
    };
    assert_eq!(corrections.len(), 1);
    assert_eq!(corrections[0].typed, "vecter");
    assert_eq!(corrections[0].instead, "vector");
}

/// The correction offered is the one people actually wrote.
///
/// `vector` and `vectar` are both one edit from `vecter`, so edit distance alone
/// cannot choose between them and dictionary order would hand back `vectar` —
/// the rarer word, and alphabetically the earlier one. Ranking by how many
/// records hold a term is what makes the suggestion useful rather than merely
/// near, and this is the only test that can tell the two rules apart.
#[test]
fn the_most_held_of_two_equally_near_terms_is_the_one_suggested() {
    let (_, (_, suggestion)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'vecter' ORDER BY id;");

    let Some(Suggestion::DidYouMean(corrections)) = suggestion else {
        panic!("a term nothing holds earned no suggestion: {suggestion:?}");
    };
    assert_eq!(
        corrections[0].instead, "vector",
        "the rarer of two equally near terms was offered"
    );
}

/// The records a misspelled query returns are the records the scan returns.
///
/// The pair the wave's framing named. A suggestion is computed only where a
/// dictionary exists, so the two paths differ in what they *advise* — and the
/// access-path rule is about what they *answer*, which is what this asserts.
#[test]
fn a_suggestion_does_not_change_which_records_a_read_answers_with() {
    let ((scanned, _), (indexed, _)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'vecter' ORDER BY id;");

    assert_eq!(
        scanned, indexed,
        "the access path decides how the answer is found and never what it is"
    );
}

/// A field with no index is not a field with nothing near it.
///
/// The three-state assertion. Both of these read as "no correction" to a caller
/// that only looks at the corrections, and they are different facts: one store
/// consulted a dictionary and found everything held, the other had no dictionary
/// to consult.
#[test]
fn a_field_with_no_index_is_not_a_field_with_nothing_near() {
    let ((_, scanned), (_, indexed)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'vector' ORDER BY id;");

    assert_eq!(
        scanned, None,
        "a read with no term dictionary reported on one it never consulted"
    );
    assert_eq!(
        indexed,
        Some(Suggestion::NothingNearer),
        "a dictionary that holds every term said nothing rather than saying so"
    );
}

/// A query that gets one word wrong and one right suggests only the wrong one.
///
/// This is the case the "no results" trigger would have missed entirely: after
/// `OR`, a query with a misspelled word still returns records, so a suggestion
/// keyed on an empty answer would stay silent exactly where a reader most needs
/// it.
#[test]
fn a_query_that_returns_records_can_still_earn_a_suggestion() {
    let (_, (records, suggestion)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'engine OR vecter' ORDER BY id;");

    assert_eq!(
        records,
        vec!["3".to_owned(), "4".to_owned()],
        "the records are the ones `engine` alone reaches"
    );
    let Some(Suggestion::DidYouMean(corrections)) = suggestion else {
        panic!("a successful read withheld a correction for a term nothing holds");
    };
    assert_eq!(corrections.len(), 1, "the held term was corrected too");
    assert_eq!(corrections[0].typed, "vecter");
}

/// An excluded term nothing holds is not a mistake.
///
/// `NOT vecter` asks for the records without that word and gets exactly those.
/// Correcting the spelling of an exclusion would be the one direction of error
/// that silently removes records the reader wanted.
#[test]
fn an_excluded_term_earns_no_suggestion() {
    let (_, (records, suggestion)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'engine NOT vecter' ORDER BY id;");

    assert_eq!(records, vec!["3".to_owned(), "4".to_owned()]);
    assert_eq!(
        suggestion,
        Some(Suggestion::NothingNearer),
        "an exclusion was corrected, which would have removed records"
    );
}

/// A term with nothing near it earns no correction, and still says a dictionary
/// was asked.
#[test]
fn a_term_near_nothing_earns_no_correction() {
    let (_, (records, suggestion)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'zebra' ORDER BY id;");

    assert!(records.is_empty());
    assert_eq!(
        suggestion,
        Some(Suggestion::NothingNearer),
        "a dictionary with nothing near reported as one that was never asked"
    );
}

/// **A typo in the first three characters is not suggestible, and that is the
/// bound rather than a bug.**
///
/// The walk this reuses is the one `MATCHES FUZZY` already runs, and it requires
/// the candidate to share a mandatory non-fuzzy prefix with the typed word.
/// `vetcor` and `vector` differ at the third character, so `vector` is not in
/// the range the walk reads at all — no edit budget would find it, because the
/// budget is never consulted.
///
/// Reusing that bound rather than inventing a looser one for suggestions is
/// deliberate: two walks over the same dictionary with two different notions of
/// "near" would eventually disagree about whether a word is a typo of another,
/// and the disagreement would surface as a suggestion for a term that
/// `MATCHES FUZZY` refuses to match. The cost is this case, and it is written
/// down here rather than discovered by a reader.
#[test]
fn a_typo_inside_the_mandatory_prefix_earns_no_correction() {
    let (_, (records, suggestion)) =
        both_paths("SELECT * FROM notes WHERE body MATCHES 'vetcor' ORDER BY id;");

    assert!(records.is_empty());
    assert_eq!(
        suggestion,
        Some(Suggestion::NothingNearer),
        "the suggestion walk found a candidate the fuzzy walk cannot reach"
    );
}
