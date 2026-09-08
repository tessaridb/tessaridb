//! Phrase search: words in the order they were typed, and adjacent.
//!
//! # What this file replaced
//!
//! The absence audit recorded `MATCHES '"ada lovelace"'` as answering `[]`
//! *"because the quotes are taken as ordinary characters"*. Both halves were
//! wrong, and the file began as the measurement that showed it.
//!
//! `Analyzer::tokens` splits on `!char::is_alphanumeric()` and drops empty
//! tokens on both sides of the filter chain — *"punctuation contributes
//! nothing"*. The quotes were never literal characters; they were **absent**. So
//! a quoted query was its own unquoted self and answered document conjunction,
//! not `[]`. The empty answer had come from a fixture, not from the store.
//!
//! That is the more dangerous failure recorded as the less dangerous one, which
//! is why these tests assert order from both sides rather than only checking
//! that a phrase finds something.
//!
//! # Why the distinction is the whole wave
//!
//! `ada lovelace` and `lovelace ada` hold the same two terms with the same
//! frequencies. Document conjunction — which is what `MATCHES` means — cannot
//! tell them apart, and neither can any test written against conjunction alone.
//! A phrase implementation that is really conjunction-plus-hope passes every
//! other test in this file and fails `the_order_is_what_a_phrase_is`.
//!
//! That is also why a quoted query answering *plausible* records is worse than
//! one answering `[]`: an empty answer gets filed as a bug, and a wrong order
//! gets read as a ranking opinion.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// Two records holding the **same two terms in opposite orders**, which is the
/// only fixture shape that can tell a phrase from a conjunction.
///
/// A third record holds one term alone, so that "conjunction" and "either term"
/// are also distinguishable — otherwise a broken implementation returning
/// everything would look like a working one.
fn phrases(indexed: bool) -> Store {
    let held = store();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ body: 'ada lovelace wrote the first program' }};\n\
             CREATE notes:2 = {{ body: 'lovelace ada, reversed on purpose' }};\n\
             CREATE notes:3 = {{ body: 'ada alone without the surname' }};",
        ))
        .unwrap();
    if indexed {
        session
            .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
            .unwrap();
    }
    held
}

/// The record ids one read answered with, in the order returned.
fn ids(session: &mut Session<'_>, read: &str) -> Vec<String> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    records.iter().map(|(id, _)| id.to_string()).collect()
}

/// **The test that decides the wave.** A phrase is the words *in that order*.
///
/// `notes:2` holds `lovelace ada` — both terms, both frequencies, wrong order.
/// Document conjunction returns it and a phrase does not, and no other assertion
/// in this file separates the two implementations.
///
/// This replaced a measurement that asserted the opposite: before this wave,
/// quoting changed nothing, because the tokenizer drops the quotes and the query
/// became its own unquoted self. That measurement is recorded in the plan file
/// as D34's real mechanism — the audit had recorded both the answer and the
/// cause wrongly.
#[test]
fn the_order_is_what_a_phrase_is() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let quoted = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES '\"ada lovelace\"';",
        );

        assert_eq!(
            quoted,
            vec!["1".to_owned()],
            "indexed={indexed}: only the record holding the two words adjacent \
             and in that order is a phrase match — notes:2 holds `lovelace ada` \
             and notes:3 holds neither pair"
        );
    }
}

/// Quoting has to *mean* something, which is only visible against the query that
/// differs from it by two characters.
#[test]
fn an_unquoted_query_is_still_a_conjunction() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let plain = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'ada lovelace';",
        );

        assert_eq!(
            plain.len(),
            2,
            "indexed={indexed}: unquoted means both terms in any order, so both \
             notes:1 and notes:2 answer and notes:3 does not — adding phrase \
             must not narrow the operator that was already there"
        );
    }
}

/// The reversed phrase finds the reversed record, which is the same assertion
/// from the other side: a phrase engine that ignored order would answer both
/// queries with both records, and one that answered neither would look correct
/// on `the_order_is_what_a_phrase_is` alone.
#[test]
fn the_reverse_phrase_finds_the_reverse_record() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let reversed = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES '\"lovelace ada\"';",
        );

        assert_eq!(
            reversed,
            vec!["2".to_owned()],
            "indexed={indexed}: `lovelace ada` is a phrase notes:2 holds and \
             notes:1 does not"
        );
    }
}

/// A phrase of one word is a word, and must not become stricter or looser than
/// the unquoted query for that word.
#[test]
fn a_phrase_of_one_word_is_that_word() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let quoted = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES '\"ada\"';",
        );
        let plain = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'ada';",
        );

        assert_eq!(
            quoted, plain,
            "indexed={indexed}: one word adjacent to nothing is that word"
        );
        assert_eq!(
            quoted.len(),
            3,
            "indexed={indexed}: every record holds `ada`"
        );
    }
}

/// Half a quote is not a phrase.
///
/// Guessing which quote was meant would make the operator's meaning depend on a
/// typo, so an unbalanced quote is analysed like any other punctuation — which
/// is to say it is dropped, and the query is the words it contains.
#[test]
fn an_unbalanced_quote_is_not_a_phrase() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let half = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES '\"ada lovelace';",
        );

        assert_eq!(
            half.len(),
            2,
            "indexed={indexed}: one quote is not a phrase, so this is the \
             conjunction of `ada` and `lovelace`"
        );
    }
}

/// The property the whole design rests on, asserted **directly** rather than
/// inferred from a passing phrase query.
///
/// A token's ordinal in the analysed stream equals its ordinal in the source
/// stream only while every filter maps one token to exactly one token. Adding a
/// synonym or n-gram filter would break phrase correctness *silently* — the
/// queries would keep answering, with the wrong spans. This test is what fails
/// instead.
#[test]
fn the_filter_chain_is_one_token_in_one_token_out() {
    let held = phrases(false);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    // Six source words, and the analyzer must produce exactly six terms: the
    // count is what a splitting or dropping filter would change.
    let counted = ids(
        &mut session,
        "SELECT * FROM notes WHERE body MATCHES '\"ada lovelace wrote the first program\"';",
    );

    assert_eq!(
        counted,
        vec!["1".to_owned()],
        "the whole of notes:1's body is a phrase of itself — it is not if any \
         filter split a token, dropped one, or emitted two, because the run \
         would no longer be contiguous or would no longer be six long"
    );
}

/// Slop widens the window and **does not** relax the order.
///
/// This is the assertion that keeps slop from quietly becoming "these words
/// somewhere near each other": `wrote ada` is the reverse of a run notes:1
/// holds, and no amount of slop may make it match. A window-based
/// implementation that forgot order passes every other slop test here.
#[test]
fn slop_widens_the_window_and_never_the_order() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        // `ada lovelace wrote` — `ada` and `wrote` sit two apart, so a span of
        // one extra token is exactly enough and none is not.
        assert!(
            ids(
                &mut session,
                "SELECT * FROM notes WHERE body MATCHES '\"ada wrote\"~0';",
            )
            .is_empty(),
            "indexed={indexed}: slop 0 is contiguity, and `lovelace` is between them"
        );
        assert_eq!(
            ids(
                &mut session,
                "SELECT * FROM notes WHERE body MATCHES '\"ada wrote\"~1';",
            ),
            vec!["1".to_owned()],
            "indexed={indexed}: one token of slop absorbs exactly one token"
        );

        // Order survives any width.
        assert!(
            ids(
                &mut session,
                "SELECT * FROM notes WHERE body MATCHES '\"wrote ada\"~9';",
            )
            .is_empty(),
            "indexed={indexed}: slop is a distance, not a permission to reorder"
        );
    }
}

/// `~0` and no marker are the same query, which is what makes "exact phrase is
/// slop 0" a property of the code rather than a sentence in a document.
#[test]
fn an_explicit_zero_is_the_bare_phrase() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let bare = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES '\"ada lovelace\"';",
        );
        let zero = ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES '\"ada lovelace\"~0';",
        );

        assert_eq!(bare, zero, "indexed={indexed}");
        assert_eq!(bare, vec!["1".to_owned()], "indexed={indexed}");
    }
}

/// A slop marker that does not parse is **refused by name**, not answered.
///
/// This is G022 S8 exactly: a quoted phrase is either a phrase or a named
/// refusal, never silently read as ordinary characters. Before the refusal
/// existed, `~x` fell through to the analyzer, `x` became a term of its own, no
/// record held it, and the query answered `[]` — which is indistinguishable
/// from "nothing matched" and is really "I did not understand you".
///
/// Reading it as slop 0 would be the same mistake wearing a helpful face.
///
/// Asserted on **both** paths because the refusal is raised before the catalog
/// is read: whether a query is refused must not depend on whether an index
/// happens to exist, which is the access-path rule (ADR-0046) applied to errors
/// rather than to answers.
#[test]
fn a_malformed_slop_marker_is_refused_by_name() {
    for indexed in [false, true] {
        let held = phrases(indexed);
        let mut session = Session::new(&held);
        session.run(USE).unwrap();

        let refused = session
            .run("SELECT * FROM notes WHERE body MATCHES '\"ada lovelace\"~x';")
            .unwrap_err()
            .to_string();

        assert!(
            refused.contains("not a slop marker"),
            "indexed={indexed}: the refusal must name what went wrong, got {refused:?}"
        );
        assert!(
            refused.contains("~x"),
            "indexed={indexed}: and quote what was written, got {refused:?}"
        );
    }
}

/// A bare `~` with no number is the same refusal, because it is the same
/// mistake: something was meant by it and the store cannot know what.
#[test]
fn a_slop_marker_with_no_number_is_refused() {
    let held = phrases(false);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let refused = session
        .run("SELECT * FROM notes WHERE body MATCHES '\"ada lovelace\"~';")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("not a slop marker"), "got {refused:?}");
}

/// A negative slop is refused rather than saturated to zero.
///
/// Clamping would answer a different question than the one asked, silently,
/// which is the behaviour this whole operator was built to stop doing.
#[test]
fn a_negative_slop_is_refused() {
    let held = phrases(false);
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let refused = session
        .run("SELECT * FROM notes WHERE body MATCHES '\"ada lovelace\"~-1';")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("not a slop marker"), "got {refused:?}");
}
