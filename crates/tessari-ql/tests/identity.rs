//! `IDENTITY` — how a table says what it names a record with.
//!
//! Kept out of `parser.rs` deliberately: that file mirrors `docs/tessariql.md`
//! statement for statement, and these assertions are about the grammar's own
//! edges rather than about an example the document carries.
//!
//! The edge that matters is that `IDENTITY` is matched as a **word**, not
//! reserved as a keyword. The grammar already does this for `INTO`, `VALUES` and
//! `ORDER BY`, and for the same reason: reserving it would make every script
//! with a field called `identity` stop parsing, which is a compatibility break
//! bought for nothing.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{Error, StatementKind, parse};
use tessari_types::IdentityKind;

fn one(source: &str) -> StatementKind {
    let parsed = match parse(source) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{source}\n  failed: {error}"),
    };
    assert_eq!(parsed.statements.len(), 1, "{source}");
    parsed.statements.into_iter().next().unwrap().kind
}

/// What a `DEFINE TABLE` or `DEFINE COLLECTION` declared about naming.
fn declared(source: &str) -> IdentityKind {
    match one(source) {
        StatementKind::DefineTable { identity, .. }
        | StatementKind::DefineCollection { identity, .. } => identity,
        other => panic!("{source} parsed as {other:?}"),
    }
}

#[test]
fn a_table_that_says_nothing_names_records_with_a_counter() {
    assert_eq!(
        declared("DEFINE TABLE users SCHEMALESS;"),
        IdentityKind::Int
    );
    assert_eq!(
        declared("DEFINE TABLE users (name string);"),
        IdentityKind::Int
    );
    assert_eq!(declared("DEFINE COLLECTION orders;"), IdentityKind::Int);
}

#[test]
fn a_table_can_ask_for_either_scheme() {
    assert_eq!(
        declared("DEFINE TABLE sessions IDENTITY uuid SCHEMALESS;"),
        IdentityKind::Uuid
    );
    assert_eq!(
        declared("DEFINE TABLE events IDENTITY int SCHEMALESS;"),
        IdentityKind::Int
    );
    assert_eq!(
        declared("DEFINE COLLECTION invitations IDENTITY uuid;"),
        IdentityKind::Uuid
    );
}

#[test]
fn the_word_is_read_regardless_of_how_it_is_cased() {
    // Every other word in this grammar is case-insensitive, and a scheme that
    // was not would refuse a script only because of how it was typed.
    assert_eq!(
        declared("define table sessions identity UUID schemaless;"),
        IdentityKind::Uuid
    );
}

#[test]
fn identity_sits_beside_the_other_flags_in_any_order() {
    assert_eq!(
        declared("DEFINE TABLE follows EDGE IDENTITY uuid SCHEMALESS;"),
        IdentityKind::Uuid
    );
    assert_eq!(
        declared("DEFINE TABLE staff IDENTITY uuid SCHEMAFULL;"),
        IdentityKind::Uuid
    );
}

#[test]
fn a_scheme_this_build_does_not_know_is_refused_by_name() {
    // Not silently the default: a table asking for something unimplemented must
    // fail at the declaration, where the author can read the message, rather
    // than mint counter ids under a name that says otherwise.
    let error = parse("DEFINE TABLE users IDENTITY ulid SCHEMALESS;").unwrap_err();
    match &error {
        Error::UnknownIdentityKind { word, .. } => assert_eq!(word, "ulid"),
        other => panic!("expected an unknown-scheme refusal, got {other:?}"),
    }
    // And it says what to write instead, because a refusal that only says "no"
    // sends the reader to the source.
    let message = error.to_string();
    assert!(message.contains("IDENTITY int"), "{message}");
    assert!(message.contains("IDENTITY uuid"), "{message}");
}

#[test]
fn identity_is_not_a_reserved_word() {
    // The reason it is matched with `eat_word`. A field, a column and a
    // parameter may all be called `identity`, and were before this existed.
    let _ = one("DEFINE FIELD identity ON users TYPE string;");
    let _ = one("DEFINE TABLE users (identity string);");
    let _ = one("SELECT identity FROM users;");
}
