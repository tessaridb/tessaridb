#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn a_name_that_could_not_stand_in_a_statement_is_refused() {
    // The only text this crate composes is table and tenancy names read back
    // out of the catalog. They came from `DEFINE` statements, so they are
    // already identifiers — this is the check that says so rather than
    // assumes it.
    assert!(plain("orders"));
    assert!(plain("orders_2026"));
    assert!(!plain(""));
    assert!(!plain("2026_orders"));
    assert!(!plain("orders; DROP TABLE users"));
    assert!(!plain("orders-live"));
}

#[test]
fn an_identity_travels_as_a_value_and_not_as_text() {
    assert_eq!(
        identity_value(&RecordId::Int(7)),
        Value::Number(tessari_types::Number::Integer(7))
    );
    assert_eq!(
        identity_value(&RecordId::Text("a'b".to_owned())),
        Value::from("a'b"),
        "a quote in an identity must stay a quote in a value"
    );
}
