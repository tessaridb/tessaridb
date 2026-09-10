//! Marking the text that matched, rather than the characters that were typed.
//!
//! # What these tests are actually guarding
//!
//! Not that something is marked. The obvious implementation — search the stored
//! text for the query string — marks something in the common case and marks
//! *nothing* in the three cases a reader most needs it: `run` does not occur in
//! `Running`, `cafe` does not occur in `Café`, and `vectr` does not occur
//! anywhere at all. Each of those is a case where the record matched, so the
//! failure appears as a returned record with no marks in it, which reads as a
//! rendering bug rather than as a wrong answer.
//!
//! So the deciding assertions here all have the same shape: the marked bytes
//! are compared against **the text**, and the text is deliberately spelled
//! differently from the query.
//!
//! # And the second failure, which no amount of marking catches
//!
//! A highlight can mark confidently and mark the wrong thing. `search::highlight`
//! takes the field alone precisely so that it cannot: the marks come from what
//! the statement asked of that field. A signature carrying its own copy of the
//! query would let the projection say `MATCHES` while the filter said
//! `MATCHES FUZZY`, and the read would return a record it matched fuzzily with
//! nothing marked in it.
//!
//! `a_fuzzy_expansion_is_marked_and_a_repeated_query_could_not_have_found_it`
//! is that assertion, and it is why the fuzzy case is not merely a third
//! spelling variation.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// Bodies chosen so that every deciding case has a text that is spelled
/// differently from the query that reaches it.
///
/// `notes:1` stems (`Running` for `run`), `notes:2` folds (`Café` for `cafe`,
/// and the accent makes the byte width differ from the character count),
/// `notes:3` is the fuzzy target, and `notes:4` holds the phrase's two terms
/// **twice** — once in the wrong order and once in the right one — which is the
/// only fixture on which marking-every-occurrence and marking-the-run differ.
fn notes(indexed: bool) -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ body: 'He was Running fast' }};\n\
             CREATE notes:2 = {{ body: 'un Café ici' }};\n\
             CREATE notes:3 = {{ body: 'a vector store' }};\n\
             CREATE notes:4 = {{ body: 'lovelace ada, and ada lovelace' }};",
        ))
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    held
}

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The one record a read answered with: its body, and the substrings its
/// highlight marked.
///
/// The marks are resolved to **text** here rather than compared as numbers,
/// because a span is only right relative to the string it indexes: an offset
/// asserted as `7..14` is a number that happens to be correct today, and
/// `"Running"` is the claim the criterion actually makes.
fn marked(session: &mut Session<'_>, read: &str) -> (String, Vec<String>) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    assert_eq!(records.len(), 1, "the fixture reads are single-record");
    let Value::Object(fields) = &records[0].1 else {
        panic!("a record is an object");
    };
    let Some(Value::String(text)) = fields.get("body") else {
        panic!("the projection names body");
    };
    let Some(Value::Array(spans)) = fields.get("marks") else {
        panic!("the projection names marks, and it answered {fields:?}");
    };
    let found = spans
        .iter()
        .map(|span| {
            let Value::Object(range) = span else {
                panic!("a mark is an object");
            };
            let (
                Some(Value::Number(Number::Integer(start))),
                Some(Value::Number(Number::Integer(end))),
            ) = (range.get("start"), range.get("end"))
            else {
                panic!("a mark carries start and end as integers, and it carried {range:?}");
            };
            let (start, end) = (
                usize::try_from(*start).unwrap(),
                usize::try_from(*end).unwrap(),
            );
            text[start..end].to_owned()
        })
        .collect();
    (text.clone(), found)
}

/// One read against a store with the search index and one without it.
///
/// Both, always. A highlight is answered from the record's own text and needs no
/// index at all — which is the claim this helper exists to keep honest, because
/// nothing else about the wave would notice if it quietly started depending on
/// one. (ADR-0046: which access path runs is decided by what exists; the answer
/// is not.)
fn both_paths(read: &str) -> ((String, Vec<String>), (String, Vec<String>)) {
    let mut answers = Vec::new();
    for indexed in [false, true] {
        let held = notes(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();
        answers.push(marked(&mut session, read));
    }
    (answers[0].clone(), answers[1].clone())
}

/// **The test that decides the wave.** The reader typed three characters and the
/// mark covers seven, because the seven are what matched.
///
/// A highlight that searched the text for the query string marks nothing here:
/// `run` does not occur in `Running`. That is the whole criterion in one
/// assertion.
#[test]
fn the_mark_covers_the_word_the_text_holds_and_not_the_one_that_was_typed() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES 'run';",
    );
    assert_eq!(scanned.1, vec!["Running".to_owned()]);
    assert_eq!(scanned, indexed, "the index may not change the answer");
    assert!(
        !scanned.0.contains("run "),
        "the fixture must not also hold the typed spelling, or this passes for the wrong reason",
    );
}

/// The folded case, and the one where a span counted in characters is short by a
/// byte and the mark stops mid-letter.
#[test]
fn a_folded_word_is_marked_across_its_real_byte_width() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES 'cafe';",
    );
    assert_eq!(scanned.1, vec!["Café".to_owned()]);
    assert_eq!(scanned, indexed);
}

/// **The second test that decides the wave.** The mark is the term the *fuzzy*
/// walk reached, which a projection repeating the query under plain `MATCHES`
/// could not have found.
///
/// This is what `search::highlight(field)` taking one argument buys: the marks
/// come from what the statement asked, so the projection cannot disagree with
/// the filter about which operator ran.
#[test]
fn a_fuzzy_expansion_is_marked_and_a_repeated_query_could_not_have_found_it() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES FUZZY 'vectr';",
    );
    assert_eq!(scanned.1, vec!["vector".to_owned()]);
    assert_eq!(scanned, indexed);
    // The word the reader typed is held by nothing, so a highlight computed from
    // the projection's own copy of the query under `MATCHES` marks nothing.
    assert!(!scanned.0.contains("vectr "));
}

/// **The third test that decides the wave.** A phrase matched once, so it is
/// marked once — even though the record holds both of its words twice.
///
/// A highlight that marks every occurrence of every phrase term passes the two
/// tests above and fails this one, which is the shape W66's own deciding case
/// had: the fixture holds `lovelace ada` *and* `ada lovelace`, so conjunction
/// and phrase cannot be told apart by term membership.
#[test]
fn a_phrase_marks_the_run_it_matched_and_not_every_word_it_named() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES '\"ada lovelace\"';",
    );
    assert_eq!(scanned.1, vec!["ada".to_owned(), "lovelace".to_owned()]);
    assert_eq!(scanned, indexed);
    // Four occurrences exist; two matched. The count is the assertion — marking
    // all four would still produce plausible-looking text.
    assert_eq!(scanned.0.matches("ada").count(), 2);
    assert_eq!(scanned.0.matches("lovelace").count(), 2);
}

/// A prefix marks the whole stored word, not the letters the reader got to.
#[test]
fn a_prefix_marks_the_word_it_reached_rather_than_the_letters_typed() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES PREFIX 'vec';",
    );
    assert_eq!(scanned.1, vec!["vector".to_owned()]);
    assert_eq!(scanned, indexed);
}

/// Marks accumulate across the predicates that named the field, because a token
/// either was reached or was not.
#[test]
fn every_predicate_on_the_field_contributes_its_own_marks() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES 'ada' AND body MATCHES PREFIX 'lovel';",
    );
    assert_eq!(
        scanned.1,
        vec![
            "lovelace".to_owned(),
            "ada".to_owned(),
            "ada".to_owned(),
            "lovelace".to_owned(),
        ],
    );
    assert_eq!(scanned, indexed);
}

/// An excluded term is never marked. It is the reason a record was *rejected*,
/// and reporting it as the reason one was returned would be exactly backwards.
#[test]
fn a_term_the_query_excludes_is_not_marked_on_the_records_that_survived() {
    let (scanned, indexed) = both_paths(
        "SELECT body, search::highlight(body) AS marks \
         FROM notes WHERE body MATCHES 'vector NOT lovelace';",
    );
    assert_eq!(scanned.1, vec!["vector".to_owned()]);
    assert_eq!(scanned, indexed);
}

/// A field nobody asked about answers no marks — because nothing was asked, not
/// because the marking failed.
///
/// Asserted rather than left to be discovered: this is the state a
/// `search::highlight` copied into a read that lost its `WHERE` lands in, and an
/// empty array is the honest answer to *what matched* when nothing was matched.
#[test]
fn a_field_nobody_asked_about_is_marked_nowhere() {
    let held = notes(true);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();
    // The analyzer is resolved — the highlight's own argument is what registers
    // the field as searched — so the empty answer is "nothing was asked" and not
    // "nothing could be read".
    let (text, found) = marked(
        &mut session,
        "SELECT body, search::highlight(body) AS marks FROM notes LIMIT 1;",
    );
    assert!(!text.is_empty());
    assert!(found.is_empty());
}

/// A record holding no text in the field answers an empty array, not `NONE`.
///
/// The fixture above cannot reach this state, because every one of its records
/// has a body. It matters on a mixed collection: if a highlight answered `NONE`
/// for the records missing the field, a projection over a table where only some
/// records carry text would come back in two different shapes, and the consumer
/// would have to tell "no text to mark" apart from "marking is unavailable".
/// Nothing matched, because there was nothing to match against — and `[]` says
/// exactly that.
///
/// The behaviour is decided *here*, in `highlight`'s own let-else chain, and not
/// by `Function::answers_for_absence`: `evaluate` intercepts this function before
/// `call` runs the absence short-circuit, so the list membership is unreachable
/// for it exactly as it is for its neighbour `search::score`. This test therefore
/// guards the shape a caller sees and would keep failing if that chain started
/// propagating absence instead.
#[test]
fn a_record_with_no_text_in_the_field_answers_no_marks_rather_than_none() {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ title: 'this record carries no body' }};",
        ))
        .unwrap();
    let outcomes = session
        .run("SELECT search::highlight(body) AS marks FROM notes;")
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    assert_eq!(records.len(), 1);
    let Value::Object(fields) = &records[0].1 else {
        panic!("a record is an object");
    };
    assert_eq!(
        fields.get("marks"),
        Some(&Value::Array(Vec::new())),
        "an absent field is marked nowhere, and it is not NONE",
    );
}
