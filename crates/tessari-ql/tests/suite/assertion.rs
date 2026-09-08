//! What `ASSERT` accepts, and what it refuses by name.
//!
//! The vocabulary is closed on purpose — the store checks assertions on its
//! apply path, so a constraint it cannot decide from the record alone is one two
//! replicas could answer differently. These tests fix the edge of that
//! vocabulary from the outside, through the parser, because the edge is the part
//! that moves when the language grows.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{Error, StatementKind, parse};
use tessari_types::{Assertion, Operand, Value};

/// The lowered constraint a `DEFINE FIELD` carries.
fn lowered(source: &str) -> Assertion {
    let parsed = match parse(source) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{source}\n  failed: {error}"),
    };
    let StatementKind::DefineField { assert, .. } =
        parsed.statements.into_iter().next().unwrap().kind
    else {
        panic!("not a field definition");
    };
    assert.expect("no assertion")
}

/// The refusal a source outside the vocabulary produces.
fn refused(source: &str) -> Error {
    match parse(source) {
        Ok(_) => panic!("{source}\n  was accepted"),
        Err(error) => error,
    }
}

#[test]
fn a_written_value_is_the_comparison_it_looks_like() {
    let Assertion::Compare { against, .. } =
        lowered("DEFINE FIELD balance ON accounts TYPE int ASSERT $value >= 0;")
    else {
        panic!("not a comparison");
    };
    assert_eq!(against, Operand::Literal(Value::from(0)));
}

#[test]
fn a_bare_name_is_a_route_into_the_record_being_checked() {
    // The same thing `WHERE ends_at > starts_at` means by the same spelling, so
    // the two read the record through one function and cannot disagree.
    let Assertion::Compare { against, .. } =
        lowered("DEFINE FIELD ends_at ON bookings TYPE datetime ASSERT $value > starts_at;")
    else {
        panic!("not a comparison");
    };
    let Operand::Field(route) = against else {
        panic!("not a route");
    };
    assert_eq!(route.to_string(), "starts_at");
}

#[test]
fn a_route_may_reach_into_a_nested_value() {
    let Assertion::Compare { against, .. } =
        lowered("DEFINE FIELD ends_at ON bookings TYPE datetime ASSERT $value > window.opens;")
    else {
        panic!("not a comparison");
    };
    let Operand::Field(route) = against else {
        panic!("not a route");
    };
    assert_eq!(route.to_string(), "window.opens");
}

#[test]
fn a_route_reaching_several_values_has_no_single_value_to_compare() {
    // Refused where it is written rather than resolving to nothing at the write
    // and becoming a constraint that always refuses.
    assert!(matches!(
        refused("DEFINE FIELD ends_at ON bookings TYPE datetime ASSERT $value > slots[*].opens;"),
        Error::AssertionNotAConstraint { .. }
    ));
}

#[test]
fn everything_still_outside_the_vocabulary_is_still_refused() {
    // Each is named separately, because "the parser rejects it" is a claim about
    // one form at a time and a single example would leave the rest to trust.
    for source in [
        // a call — not a pure function of the record
        "DEFINE FIELD seen ON ledgers TYPE datetime ASSERT $value < time::now();",
        // any parameter but the one the store binds
        "DEFINE FIELD cap ON ledgers TYPE int ASSERT $limit > 3;",
        // arithmetic, even over a route
        "DEFINE FIELD ends_at ON bookings TYPE int ASSERT $value > starts_at + 1;",
        // the mirror spelling, which flipping is only correct for some operators
        "DEFINE FIELD balance ON ledgers TYPE int ASSERT 0 <= $value;",
        // a bare literal, which constrains nothing
        "DEFINE FIELD balance ON ledgers TYPE int ASSERT true;",
    ] {
        assert!(
            matches!(refused(source), Error::AssertionNotAConstraint { .. }),
            "{source}"
        );
    }
}
