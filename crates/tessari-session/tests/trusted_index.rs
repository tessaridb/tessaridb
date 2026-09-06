//! When a search index is believed instead of re-tested, and what has to hold
//! first.
//!
//! # What changed, and why it needs tests of its own
//!
//! An index-served read produces *candidates*: the condition is then evaluated
//! against each record, and that re-test is what makes adding an index unable to
//! change what a query returns. For one shape — a plain conjunction of terms,
//! which is what `MATCHES 'a b'` is — the postings are derived by the same
//! `analyzer.terms` over the same field that the predicate calls, so their
//! intersection *is* the answer and re-testing re-analyses the record's whole
//! text to reach a verdict already reached. On a word most records hold, that
//! re-analysis is the entire cost of the query.
//!
//! Believing the read is therefore permitted, and only where believing it and
//! re-testing it produce the same records. Each condition below is a way the
//! re-test was carrying something other than the predicate:
//!
//! **The clause has to be the whole condition** — anything joined with `AND`
//! still has to be evaluated. Covered by the store's existing scan/index pair
//! tests, which run every query both ways.
//!
//! **The field has to be one the session may read.** A field permission removes
//! the field from the record *before* anything looks at it, so on an
//! index-served read it is the re-test that enforces the redaction — the record
//! the candidate is re-tested against is the redacted one. That is asserted
//! here, because the failure it prevents is not a wrong row count: it is a
//! reader learning the contents of a field they were never granted, one query at
//! a time, from which rows come back.
//!
//! **The read has to account for the transaction's own writes**, and this one
//! was a wrong answer already, before any of the above. Index entries are
//! derived at commit, so a pending write has no posting at all. The equality,
//! string-prefix and region reads each settle that by asking every pending
//! record directly after the walk; the two term reads never did. A record
//! updated *into* a match inside its own transaction was therefore missed — no
//! posting, no candidate, and nothing a re-test could add back — from a read
//! that reported itself served by an index.
//!
//! The term reads now settle pending writes the same way, so both directions
//! are asserted here **and** the access path is asserted with them: answering
//! correctly by quietly falling back to a scan and answering correctly through
//! the index are the same answer, and only one of them is what these tests are
//! about.
//!
//! The semantic conditions — a phrase is not a conjunction, an excluded term
//! names a complement an index cannot enumerate — are asserted where they
//! already were, in `phrase.rs` and `boolean.rs`, which run every query with an
//! index and without one and compare the records. Those tests are what would
//! fail if this change had claimed too much.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A store holding two notes, a search index over their text, and an editor who
/// has not been given the text field.
fn peopled(store: &Store) {
    let mut opening = Session::new(store);
    opening
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD text ON notes TYPE string ANALYZER english;\n\
             DEFINE INDEX notes_text ON notes FIELDS text SEARCH;\n\
             CREATE notes:1 = { title: 'first', text: 'the analyzer folds accents' };\n\
             CREATE notes:2 = { title: 'second', text: 'a vector index walks a graph' };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The record ids the one read in a script answered with.
///
/// The read is found rather than taken from the end, because the transaction
/// scripts below close with `VERIFY` and the outcome of that is `Done`.
///
/// `VERIFY` rather than `COMMIT` on purpose, and the distinction is worth
/// stating because getting it backwards makes a test report writes vanishing:
/// `VERIFY` runs every check a commit runs and then **discards** the work. What
/// these tests assert happens *inside* the transaction, so discarding it at the
/// end is exactly right — nothing is left behind for the next assertion to
/// stand on by accident.
fn ids(session: &mut Session<'_>, statement: &str) -> Vec<String> {
    read(session, statement).0
}

/// The record ids, and the access path the read took.
///
/// The second half matters in the transaction tests: answering correctly by
/// falling back to a scan and answering correctly through the index are the same
/// answer, and only one of them is the thing being asserted.
fn read(session: &mut Session<'_>, statement: &str) -> (Vec<String>, AccessPath) {
    let answered = session.run(statement).unwrap();
    let mut found = answered.iter().filter_map(|outcome| match outcome {
        Outcome::Records { records, plan, .. } => Some((records, plan)),
        _ => None,
    });
    let (records, plan) = found.next().expect("one read in the script");
    assert!(found.next().is_none(), "one read in the script");
    (
        records.iter().map(|(id, _)| id.to_string()).collect(),
        plan.access,
    )
}

#[test]
fn a_field_nobody_granted_is_not_searchable_through_its_own_index() {
    // The one that matters. `text` is indexed and `ada` may read `title` only,
    // so the record she is handed has no `text` at all and the predicate is
    // false by the missing-field rule. An index that were believed here would
    // answer from postings derived at commit — from the *unredacted* record —
    // and every query would report whether the hidden field holds a word.
    let held = store();
    peopled(&held);
    signed_in(&held, "root")
        .run("GRANT read ON notes FIELDS title TO ada;")
        .unwrap();

    let mut ada = signed_in(&held, "ada");
    assert!(
        ids(
            &mut ada,
            "SELECT id FROM notes WHERE text MATCHES 'analyzer';"
        )
        .is_empty(),
        "a redacted field answers nothing, index or no index"
    );
    // The complement, so the test cannot pass because the fixture is empty: the
    // same session reads the same table on a field it was granted.
    assert_eq!(
        ids(&mut ada, "SELECT id FROM notes WHERE title = 'first';").len(),
        1
    );
}

#[test]
fn granting_the_indexed_field_makes_it_searchable_again() {
    // The other half of the redaction test, and the reason it is a pair: an
    // assertion that a query answers nothing passes on a store that answers
    // nothing to everything.
    let held = store();
    peopled(&held);
    signed_in(&held, "root")
        .run("GRANT read ON notes FIELDS title, text TO ada;")
        .unwrap();

    let mut ada = signed_in(&held, "ada");
    assert_eq!(
        ids(
            &mut ada,
            "SELECT id FROM notes WHERE text MATCHES 'analyzer';"
        ),
        vec!["1".to_owned()]
    );
}

#[test]
fn a_record_rewritten_inside_the_transaction_is_read_from_its_new_text() {
    // Entries are derived at commit, so inside the transaction the posting for
    // `analyzer` still points at this record while its payload no longer holds
    // the word. The read has to answer from the payload.
    let held = store();
    peopled(&held);
    let mut root = signed_in(&held, "root");
    let (answered, access) = read(
        &mut root,
        "BEGIN;\n\
         UPDATE notes:1 = { title: 'first', text: 'a graph of vectors' };\n\
         SELECT id FROM notes WHERE text MATCHES 'analyzer';\n\
         VERIFY;",
    );
    assert_eq!(
        access,
        AccessPath::Index,
        "through the index, not around it"
    );
    assert!(
        answered.is_empty(),
        "the word was written away in this transaction: {answered:?}"
    );
}

#[test]
fn a_record_written_into_the_match_inside_the_transaction_is_found() {
    // The same seam from the other side, and the direction that was silently
    // wrong: this record has no posting for the word yet — entries are derived
    // at commit — so before the term reads settled pending writes there was no
    // candidate to produce and no re-test that could add one back. A missing
    // row, from a read that reported itself served by an index.
    let held = store();
    peopled(&held);
    let mut root = signed_in(&held, "root");
    let (answered, access) = read(
        &mut root,
        "BEGIN;\n\
         UPDATE notes:2 = { title: 'second', text: 'the analyzer folds accents' };\n\
         SELECT id FROM notes WHERE text MATCHES 'analyzer';\n\
         VERIFY;",
    );
    assert_eq!(
        access,
        AccessPath::Index,
        "through the index, not around it"
    );
    assert_eq!(answered.len(), 2, "both records hold the word now");
}
