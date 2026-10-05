#![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

use super::*;
use std::collections::BTreeMap;
use tessari_types::json::Malformed;
use tessari_types::{Number, Value};

fn parsed(text: &str) -> Value {
    read(text.as_bytes()).unwrap_or_else(|failure| panic!("{text} did not read: {failure}"))
}

fn refused(text: &str) -> Malformed {
    read(text.as_bytes()).expect_err("this should not have read")
}

/// Compare two answers by their written form rather than by `PartialEq`.
///
/// `Value`'s equality equates `Integer(3)`, `Float(3.0)` and
/// `Decimal("3.0")` deliberately, and the join relies on it. So an
/// assertion about which KIND this reader answers cannot be written with
/// `assert_eq!` on the values themselves: it passes against a reader that
/// answered the other kind, which is the one difference the tests below
/// exist to catch and the one a caller sees on the wire (Q-76).
#[track_caller]
fn same(answered: impl core::fmt::Debug, expected: impl core::fmt::Debug) {
    assert_eq!(format!("{answered:?}"), format!("{expected:?}"));
}

#[test]
fn a_whole_number_stays_whole() {
    // The decision this reader exists for. A millisecond timestamp read as a
    // double comes back rounded, and the record lands with the wrong value
    // and no error anywhere.
    same(
        parsed("1756300000000"),
        Value::Number(Number::Integer(1_756_300_000_000)),
    );
    same(parsed("-7"), Value::Number(Number::Integer(-7)));
    same(parsed("0"), Value::Number(Number::Integer(0)));
}

#[test]
fn a_number_written_with_a_point_or_an_exponent_is_a_float() {
    // Left on `assert_eq!` deliberately. `1.5` has no integer twin and
    // this reader has no decimal path, so there is no kind this assertion
    // could fail to see — converting it would add noise around the two
    // below, which have one (Q-76).
    assert_eq!(parsed("1.5"), Value::Number(Number::Float(1.5)));
    same(parsed("1e3"), Value::Number(Number::Float(1000.0)));
    // Written as a float even though its value is whole: what a producer
    // wrote is what it meant, and `2.0` in a payload is a measurement.
    same(parsed("2.0"), Value::Number(Number::Float(2.0)));
}

#[test]
fn an_integer_too_large_for_i64_degrades_rather_than_stalling_a_partition() {
    // Losing precision on a number nobody indexes is worse than a refusal
    // only in theory; in production a refusal here blocks the partition.
    let Value::Number(Number::Float(held)) = parsed("99999999999999999999999") else {
        panic!("not a float");
    };
    assert!(held > 1e22);
}

#[test]
fn null_is_null_and_not_absent() {
    // This store tells `null` and `none` apart and JSON has one word. A
    // producer that wrote the key meant *present, holding nothing*.
    assert_eq!(parsed("null"), Value::Null);
}

#[test]
fn an_object_reads_into_fields_and_nesting_works() {
    let Value::Object(fields) = parsed(r#"{"a":1,"b":{"c":"x"}}"#) else {
        panic!("not an object");
    };
    same(fields.get("a"), Some(&Value::Number(Number::Integer(1))));
    let Some(Value::Object(inner)) = fields.get("b") else {
        panic!("not nested");
    };
    assert_eq!(inner.get("c"), Some(&Value::from("x")));
}

#[test]
fn the_escapes_resolve_including_a_surrogate_pair() {
    assert_eq!(parsed(r#""a\nb""#), Value::from("a\nb"));
    assert_eq!(parsed(r#""A""#), Value::from("A"));
    // Outside the basic plane, which is where a naive reader emits two
    // replacement characters and nobody notices until an emoji arrives.
    assert_eq!(parsed(r#""😀""#), Value::from("\u{1F600}"));
}

#[test]
fn a_lone_surrogate_is_refused_rather_than_replaced() {
    // Replacing it would put U+FFFD in a record and lose which character was
    // meant, with nothing anywhere saying so.
    assert_eq!(
        refused(r#""\ud83d""#).reason,
        "a high surrogate with no pair"
    );
    assert_eq!(
        refused(r#""\udc00""#).reason,
        "a low surrogate with no high one before it"
    );
}

#[test]
fn multibyte_text_survives_the_round_trip() {
    assert_eq!(parsed(r#""привет 🙂""#), Value::from("привет 🙂"));
}

#[test]
fn a_payload_holding_two_values_is_refused_rather_than_half_read() {
    // A framing mistake. Reading the first value would apply half a message
    // and report success.
    assert_eq!(
        refused("{} {}").reason,
        "unexpected content after the value"
    );
}

#[test]
fn nesting_is_bounded_so_a_message_cannot_overflow_the_stack() {
    // The bytes come off a broker, so the depth is attacker-controlled. An
    // overflow aborts the process, taking every other consumer and every
    // open connection with it — which is not a failure any `on_failure`
    // policy could have caught.
    let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
    assert_eq!(refused(&deep).reason, "nested too deeply");
}

#[test]
fn the_ordinary_malformed_payloads_all_say_where() {
    for (text, reason) in [
        ("", "the payload ended where a value was expected"),
        ("{", "a field name must be a string"),
        (r#"{"a""#, "a field name must be followed by `:`"),
        (r#"{"a":1"#, "expected `,` or `}`"),
        ("[1", "expected `,` or `]`"),
        (r#""unterminated"#, "the payload ended inside a string"),
        ("tru", "not the start of a value"),
        ("-", "a number with no digits"),
    ] {
        let failure = refused(text);
        assert_eq!(failure.reason, reason, "for {text:?}");
    }
}

#[test]
fn an_empty_object_and_an_empty_array_read() {
    assert_eq!(parsed("{}"), Value::Object(BTreeMap::new()));
    assert_eq!(parsed("[]"), Value::Array(Vec::new()));
    assert_eq!(parsed("  { }  "), Value::Object(BTreeMap::new()));
}

#[test]
fn invalid_utf8_in_a_string_is_refused() {
    // A byte sequence that is not text cannot become a string field, and
    // passing it through would put bytes in a record no reader expects.
    let payload = [b'"', 0xFF, 0xFE, b'"'];
    assert_eq!(
        read(&payload).expect_err("read invalid utf-8").reason,
        "a string that is not valid UTF-8"
    );
}
