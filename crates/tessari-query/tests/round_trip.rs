//! The property that makes a builder worth having.
//!
//! A builder that emits text is a second dialect of the language, free to drift
//! from the parser until a caller's query is refused in production. This file is
//! what makes that drift impossible to introduce quietly: every query built here
//! is rendered, parsed back by the **real** parser, and compared to what was
//! built.
//!
//! # Both halves, because neither is sufficient
//!
//! - `erase_spans(parse(render(built))) == erase_spans(built)` — the syntax
//!   survived the trip. On its own it would not notice a renderer whose output
//!   re-parses to the right tree but does not render the same way twice.
//! - `render(parse(render(built))) == render(built)` — the text is a fixpoint.
//!   On its own it would pass for a renderer that is not injective: two
//!   different trees rendering to one text would satisfy it happily.
//!
//! Spans are erased rather than compared because a **built** statement has no
//! source text, so it has no byte offsets to survive with. The normalisation
//! lives in the parser crate's test support and does not touch the production
//! `PartialEq` — see `ADR-0022`.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use tessari_ql::test_support::erase_spans;
use tessari_ql::{BinaryOp, parse, render};
use tessari_query::{Query, select};
use tessari_types::Value;

/// Build, render, parse back, and compare both ways.
fn survives(query: Query) -> String {
    let built = query.script;
    let text = render(&built).expect("a built query renders");

    let mut parsed = parse(&text).expect("rendered text parses");
    let mut original = built.clone();
    erase_spans(&mut parsed);
    erase_spans(&mut original);
    assert_eq!(parsed, original, "syntax did not survive: {text}");

    let again =
        render(&parse(&text).expect("rendered text parses")).expect("the parsed tree renders");
    assert_eq!(again, text, "text is not a fixpoint: {text}");

    text
}

#[test]
fn the_whole_table() {
    assert_eq!(
        survives(select().from("users").build().unwrap()),
        "SELECT * FROM users;"
    );
}

#[test]
fn named_projections_carry_their_names() {
    let text = survives(
        select()
            .field("name")
            .field("address.city")
            .from("users")
            .build()
            .unwrap(),
    );
    assert_eq!(
        text,
        "SELECT name AS name, address.city AS city FROM users;"
    );
}

#[test]
fn a_filter_becomes_a_parameter_and_a_where() {
    let query = select()
        .from("users")
        .filter(
            "email",
            BinaryOp::Equal,
            Value::String("ada@example.com".to_owned()),
        )
        .build()
        .unwrap();
    assert_eq!(
        query.parameters.get("p0"),
        Some(&Value::String("ada@example.com".to_owned()))
    );
    assert_eq!(survives(query), "SELECT * FROM users WHERE (email = $p0);");
}

#[test]
fn several_filters_compose_with_and() {
    let query = select()
        .from("users")
        .filter("city", BinaryOp::Equal, Value::String("Paris".to_owned()))
        .filter(
            "tags",
            BinaryOp::Contains,
            Value::String("urgent".to_owned()),
        )
        .build()
        .unwrap();
    assert_eq!(query.parameters.len(), 2);
    assert_eq!(
        survives(query),
        "SELECT * FROM users WHERE ((city = $p0) AND (tags CONTAINS $p1));"
    );
}

#[test]
fn every_optional_clause_at_once() {
    let query = select()
        .field("name")
        .from("users")
        .filter("age", BinaryOp::GreaterOrEqual, Value::Number(18.into()))
        .order_by("name", false)
        .order_by("address.city", true)
        .start(20)
        .limit(10)
        .build()
        .unwrap();
    assert_eq!(
        survives(query),
        "SELECT name AS name FROM users WHERE (age >= $p0) \
         ORDER BY name, address.city DESC START 20 LIMIT 10;"
    );
}

#[test]
fn a_route_through_a_position_survives() {
    let query = select()
        .from("users")
        .filter(
            "history[0].by.name",
            BinaryOp::Equal,
            Value::String("ada".to_owned()),
        )
        .build()
        .unwrap();
    assert_eq!(
        survives(query),
        "SELECT * FROM users WHERE (history[0].by.name = $p0);"
    );
}

/// Criterion S2, stated as its own falsification.
///
/// The value is the classic injection, and the assertion is not that it is
/// escaped — it is that **no part of it is in the query text at all**. There is
/// nothing to escape, because the value never became grammar.
#[test]
fn an_injection_lands_in_the_parameter_map_and_never_in_the_script() {
    let attack = "'; DROP TABLE users; --";
    let query = select()
        .from("users")
        .filter("name", BinaryOp::Equal, Value::String(attack.to_owned()))
        .build()
        .unwrap();

    assert_eq!(
        query.parameters.get("p0"),
        Some(&Value::String(attack.to_owned())),
        "the value must reach the store as a value"
    );

    let text = survives(query);
    assert!(
        !text.contains(attack),
        "the whole value is in the text: {text}"
    );
    assert!(
        !text.contains("DROP"),
        "a fragment of the value is in the text: {text}"
    );
    assert!(
        !text.contains(';') || text.ends_with(';'),
        "an extra statement was opened: {text}"
    );
    assert_eq!(text, "SELECT * FROM users WHERE (name = $p0);");
}

/// The other half of S2: a **name** is grammar, so it is refused rather than
/// quoted. A builder that quoted it would be inventing a convention the
/// language does not have.
#[test]
fn a_name_that_is_not_a_name_is_refused() {
    let refused = select().from("users; DROP TABLE secrets").build();
    assert!(refused.is_err(), "an injected table name was accepted");

    let refused = select()
        .from("users")
        .filter("name = 'x' OR 1", BinaryOp::Equal, Value::Bool(true))
        .build();
    assert!(refused.is_err(), "an injected route was accepted");

    let refused = select().field("name; DROP TABLE x").from("users").build();
    assert!(refused.is_err(), "an injected projection was accepted");
}

/// The first mistake is the one reported, so a later one cannot hide it.
#[test]
fn the_first_failure_is_the_one_reported() {
    let refused = select()
        .field("first bad")
        .field("second bad")
        .from("users")
        .build()
        .unwrap_err();
    assert!(format!("{refused}").contains("first bad"), "{refused}");
}
