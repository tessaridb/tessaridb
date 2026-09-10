//! The renderer, and the span normalisation the round trip is compared with.
//!
//! Two things are checked here that `tessari-query`'s round trip cannot reach.
//!
//! **The coverage boundary names itself.** `SELECT` is the only form this
//! milestone writes back out, and every other form must say which it was rather
//! than fall into a catch-all.
//!
//! **`erase_spans` reaches every span.** The round trip in the builder crate
//! only ever compares trees the *builder* can produce, and the builder emits
//! neither a fold nor a function call — so the two span sites that live as enum
//! variant fields, `ExprKind::Fold` and `ExprKind::Call`, would go untested
//! there. They are exactly the two a search for `pub span: Span` cannot see, and
//! the count in `ADR-0022` was wrong because of it. The test below is what makes
//! the correction a checked fact rather than a claim.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use tessari_ql::test_support::erase_spans;
use tessari_ql::{Script, parse, render};

fn parsed(source: &str) -> Script {
    parse(source).expect(source)
}

fn erased(source: &str) -> Script {
    let mut script = parsed(source);
    erase_spans(&mut script);
    script
}

/// Shift every byte offset in the script, without changing what it says.
fn shifted(source: &str) -> String {
    format!("     {source}")
}

/// The property `erase_spans` exists for, and its own control.
///
/// Two texts that say the same thing at different offsets must compare equal
/// once spans are erased — and *unequal* before, or the shift proved nothing and
/// the test would pass for a normalisation that did nothing at all.
fn position_stops_mattering(source: &str) {
    let moved = shifted(source);
    assert_ne!(
        parsed(source),
        parsed(&moved),
        "the shift did not move any offset, so this proves nothing: {source}"
    );
    assert_eq!(
        erased(source),
        erased(&moved),
        "a span survived erasure: {source}"
    );
}

#[test]
fn erasure_reaches_an_ordinary_read() {
    position_stops_mattering("SELECT name AS name FROM users WHERE (age >= $p0);");
}

/// `ExprKind::Fold` holds its span as a variant field, which carries no `pub`.
#[test]
fn erasure_reaches_the_span_inside_a_fold() {
    position_stops_mattering("SELECT count(*) AS n FROM users;");
}

/// `ExprKind::Call` holds its span the same way.
#[test]
fn erasure_reaches_the_span_inside_a_call() {
    position_stops_mattering("SELECT string::upper(city) AS shout FROM users;");
}

#[test]
fn erasure_reaches_a_nested_read() {
    position_stops_mattering("CREATE audit:1 = { target: (SELECT * FROM users:1) };");
}

#[test]
fn a_read_renders_back_to_itself() {
    let source = "SELECT * FROM users WHERE (email = $token) LIMIT 10;";
    assert_eq!(render(&parsed(source)).unwrap(), source);
}

#[test]
fn an_unrendered_statement_names_itself() {
    for (source, expected) in [
        ("BEGIN;", "BEGIN"),
        ("CREATE users:1 = { name: 'ada' };", "CREATE"),
        ("DELETE users:1;", "DELETE"),
        ("INFO FOR TABLE users;", "INFO"),
        ("DROP TABLE users;", "DROP TABLE"),
    ] {
        let refused = render(&parsed(source)).unwrap_err();
        let message = format!("{refused}");
        assert!(message.starts_with(expected), "{source} said {message}");
    }
}

#[test]
fn an_unrendered_access_path_names_itself() {
    let refused = render(&parsed("SELECT * FROM users:1;")).unwrap_err();
    assert!(format!("{refused}").contains("one record"), "{refused}");
}

#[test]
fn the_statement_render_refuses_to_print_is_not_printed_by_debug_either() {
    // `render` refuses `DEFINE USER` so that a credential cannot be recovered
    // from a statement the store is holding. A derived `Debug` gave it back in
    // one interpolation, which is a guard implemented at one of the two places
    // that stringify the tree. Nothing prints a statement today; the line that
    // does is always somewhere else and written later.
    let secret = "correct horse battery staple";
    let script = parsed(&format!("DEFINE USER ada ROLE editor PASSWORD '{secret}';"));

    assert!(render(&script).is_err(), "render must still refuse it");

    let printed = format!("{script:?}");
    assert!(!printed.contains(secret), "{printed}");
    assert!(printed.contains("<redacted>"), "{printed}");
    assert!(
        printed.contains("ada"),
        "the name is the value of printing one: {printed}"
    );
}
