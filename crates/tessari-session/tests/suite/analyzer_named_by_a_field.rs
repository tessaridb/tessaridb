//! A field may not name an analyzer nobody declared (Q-235).
//!
//! # The asymmetry this closes
//!
//! The store already defends this link from one end: `DROP ANALYZER` is refused
//! while a field still names the analyzer, because the reference is by **name**
//! and nothing in the catalog enforces it — what a dangling one produces is a
//! search that quietly stops matching, a wrong answer indistinguishable from a
//! right one.
//!
//! Declaring the same dangling reference was accepted. So the guard on the drop
//! side had a shorter route around it: misspell the analyzer once at declaration
//! time and the field is permanently in the state the drop-side refusal exists
//! to prevent.
//!
//! # Why the refusal belongs at declaration time
//!
//! `define_field` already evaluates and type-checks `DEFAULT` where it is
//! written rather than where it first bites, and says why in its own doc
//! comment. The analyzer sat three lines from that argument, unresolved. It is
//! the same catalog lookup `DROP ANALYZER` performs in the other direction.
//!
//! # What each test decides
//!
//! Asserting only that the statement errors would miss the half that matters: a
//! refusal that still wrote the field would error and be wrong. So the deciding
//! assertion in each case is about the **catalog after the refusal**, observed
//! through a strict table — a field that was never created is a field a write
//! cannot use.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PLACE: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

/// A store with a tenancy, one declared analyzer, and one strict table.
///
/// The table is strict because that is what makes the absence of a field
/// observable: a write naming an undeclared field is refused, so "the refusal
/// wrote nothing" is a behaviour rather than an introspection detail.
fn ready() -> Store {
    let held = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&held);
    session
        .run(&format!(
            "{PLACE}\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes (title string);",
        ))
        .unwrap();
    held
}

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";

/// **The test Q-235 asks for.** The declaration is refused, and it names the
/// analyzer it could not find.
#[test]
fn a_field_naming_an_undeclared_analyzer_is_refused() {
    let held = ready();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let refused = session.run("DEFINE FIELD body ON notes TYPE string ANALYZER missing;");
    let error = refused.expect_err("a field may not name an analyzer that does not exist");

    let said = error.to_string();
    assert!(
        said.contains("missing"),
        "the refusal must name the analyzer it could not find, and it said: {said}",
    );
    assert!(
        said.contains("analyzer"),
        "the refusal must say what kind of thing was missing, and it said: {said}",
    );
}

/// **The deciding assertion.** A refused declaration leaves no field behind.
///
/// Without this, a store that refused the statement *after* writing the field
/// would pass the test above while leaving the catalog in exactly the state the
/// refusal exists to prevent.
#[test]
fn a_refused_declaration_writes_no_field() {
    let held = ready();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    session
        .run("DEFINE FIELD body ON notes TYPE string ANALYZER missing;")
        .expect_err("the declaration must be refused");

    // The table is strict, so an undeclared field cannot be written. If the
    // refusal had created `body`, this would succeed.
    session
        .run("CREATE notes:1 = { title: 'first', body: 'text' };")
        .expect_err("`body` must not exist, so writing it must be refused");
}

/// **The control.** With the analyzer declared, the same declaration succeeds
/// and the field works — so the refusals above are about the missing name and
/// not about `ANALYZER` having stopped being accepted.
#[test]
fn a_field_naming_a_declared_analyzer_is_accepted() {
    let held = ready();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    session
        .run("DEFINE FIELD body ON notes TYPE string ANALYZER simple;")
        .unwrap();
    session
        .run("CREATE notes:1 = { title: 'first', body: 'text' };")
        .unwrap();
}

/// The columnar spelling is the same declaration written differently, so it
/// refuses the same name — the two spellings agreeing is the property the
/// desugaring exists to hold.
#[test]
fn the_columnar_spelling_refuses_the_same_name() {
    let held = ready();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    let refused = session.run("DEFINE TABLE drafts (body string ANALYZER missing);");
    let error = refused.expect_err("a column may not name an analyzer that does not exist");
    assert!(
        error.to_string().contains("missing"),
        "the refusal must name the analyzer, and it said: {error}",
    );
}

/// `ALTER FIELD` drops and redeclares in one commit, so a refused redeclaration
/// must leave the **original** field standing rather than the removal.
///
/// This is the case with the sharpest failure mode: an alteration that took the
/// drop and then refused the replacement would delete a working field on a
/// typo.
#[test]
fn a_refused_alteration_keeps_the_field_it_was_altering() {
    let held = ready();
    let mut session = Session::new(&held);
    session.run(USE).unwrap();

    session
        .run("DEFINE FIELD body ON notes TYPE string ANALYZER simple;")
        .unwrap();

    session
        .run("ALTER TABLE notes ALTER FIELD body TYPE string ANALYZER missing;")
        .expect_err("the alteration must be refused");

    // The original declaration must still be in force: the field exists and the
    // strict table accepts a write naming it.
    session
        .run("CREATE notes:1 = { title: 'first', body: 'text' };")
        .unwrap();
}
