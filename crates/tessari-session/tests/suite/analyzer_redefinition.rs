//! Redefining an analyzer against a populated index (G014 F6).
//!
//! # The property, and why "it errors" is not the whole of it
//!
//! F6 says changing what a field's text becomes cannot **silently** change what
//! it matches: a redefinition either reindexes or is refused, and the store says
//! which. This store refuses — `DEFINE ANALYZER` reserves the catalog name, and a
//! second definition of the same name fails on that reservation.
//!
//! Asserting only that the statement errors would miss the half that matters. A
//! refusal that still wrote *part* of the new definition, or that left the index
//! answering under a chain nobody can name any more, would error and be wrong.
//! So the deciding assertion here is not the error — it is that the same query
//! answers the same records **after** the refusal as before it, which is the
//! property F6 is actually about.
//!
//! # Why the index has to be populated first
//!
//! An empty index cannot tell the two branches apart: reindexing nothing and
//! refusing to reindex both leave nothing. The fixture therefore writes records
//! and builds the SEARCH index *before* the redefinition is attempted, so that a
//! store which quietly accepted the new chain would answer differently — the
//! stemmer is the difference, and `run` reaching `Running` is what depends on it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// A populated, indexed collection whose field declares a **stemming** analyzer.
///
/// The stemmer is the load-bearing filter: it is what makes `run` reach
/// `Running`, so a chain that lost it answers differently and the test can see
/// the difference.
fn stemming_store() -> Store {
    let held = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ body: 'He was Running fast' }};\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
        ))
        .unwrap();
    held
}

/// How many records `body MATCHES <query>` answers with.
fn found(session: &mut Session<'_>, query: &str) -> usize {
    let read = format!("SELECT body FROM notes WHERE body MATCHES '{query}';");
    let outcomes = session.run(&read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    records.len()
}

/// **The test that decides F6.** A redefinition is refused, and the index still
/// answers under the chain it was built with.
///
/// The second assertion is the one with teeth. A store that accepted the new
/// two-filter chain would stop reaching `Running` from `run`, because the
/// stemmer is gone — and it would do so without touching the postings, which is
/// exactly the silent divergence F6 forbids: an index answering under one
/// analyzer while the catalog names another.
#[test]
fn redefining_an_analyzer_over_a_populated_index_is_refused_and_changes_no_answer() {
    let held = stemming_store();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    // Before: the stemmer is doing its job.
    assert_eq!(found(&mut session, "run"), 1);

    // A different chain under the same name — the stemmer dropped.
    let refused = session.run("DEFINE ANALYZER english FILTERS lowercase;");
    let error = refused.expect_err("redefining a declared analyzer must not succeed");

    // It says which of the two branches happened, and it names the thing.
    let said = error.to_string();
    assert!(
        said.contains("english"),
        "the refusal must name the analyzer it refused, and it said: {said}",
    );

    // After: unchanged. This is the property, not the error above.
    assert_eq!(
        found(&mut session, "run"),
        1,
        "a refused redefinition must leave the index answering under the original chain",
    );
}

/// `IF NOT EXISTS` is the spelling that means "leave whatever is there", and it
/// leaves it — including the filter chain, which is the part a caller who
/// re-runs a schema script is relying on without noticing.
#[test]
fn if_not_exists_keeps_the_declared_chain_rather_than_replacing_it() {
    let held = stemming_store();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    session
        .run("DEFINE ANALYZER IF NOT EXISTS english FILTERS lowercase;")
        .unwrap();

    assert_eq!(
        found(&mut session, "run"),
        1,
        "IF NOT EXISTS answered ok, so the original stemming chain must still be the one in force",
    );
}

/// **The control.** Without the stemmer, `run` reaches nothing — so the
/// "answers unchanged" assertions above are not passing vacuously.
///
/// This is the test that makes the other two mean something. If a lowercase-only
/// chain also answered `run` with `Running`, then "the index still answers the
/// same" would hold no matter which analyzer won, and F6 would be untested while
/// looking tested.
#[test]
fn without_the_stemmer_the_same_query_reaches_nothing() {
    let held = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER english FILTERS lowercase;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER english;\n\
             CREATE notes:1 = {{ body: 'He was Running fast' }};\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
        ))
        .unwrap();

    assert_eq!(
        found(&mut session, "run"),
        0,
        "the stemmer is what makes `run` reach `Running`; if this finds a record, \
         the redefinition tests above prove nothing",
    );
    // And the chain that is in force does still work, so the zero above is the
    // stemmer's absence and not a broken fixture.
    assert_eq!(found(&mut session, "running"), 1);
}

/// A *different* name is not a redefinition, and must still be allowed — the
/// refusal above is about the name, not about analyzers being immutable as a
/// class.
///
/// Without this, a store that refused every `DEFINE ANALYZER` after the first
/// would pass the test above for entirely the wrong reason.
#[test]
fn a_second_analyzer_under_a_new_name_is_not_a_redefinition() {
    let held = stemming_store();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    session
        .run("DEFINE ANALYZER plain FILTERS lowercase;")
        .unwrap();

    assert_eq!(found(&mut session, "run"), 1);
}
