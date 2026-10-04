#![allow(clippy::panic)]

use tessari_ql::{Function, Span};
use tessari_types::{Number, Value};

use super::call;

fn at() -> Span {
    Span::new(0, 1)
}

fn text(value: &str) -> Value {
    Value::from(value)
}

/// Compare two answers by their written form rather than by `PartialEq`.
///
/// `Value`'s equality equates `Integer(3)`, `Float(3.0)` and
/// `Decimal("3.0")` deliberately, and the join relies on it. So an
/// assertion about which KIND a function answers cannot be written with
/// `assert_eq!` on the values themselves: it passes against a function
/// answering the other kind, and a kind is a difference the caller sees on
/// the wire (Q-76). The site that carries a message writes the comparison
/// out instead, so the message survives.
#[track_caller]
fn same(answered: impl core::fmt::Debug, expected: impl core::fmt::Debug) {
    assert_eq!(format!("{answered:?}"), format!("{expected:?}"));
}

#[test]
fn a_length_counts_characters_and_not_bytes() {
    let answer = call(Function::StringLen, &[text("héllo")], at()).expect("a length");
    same(answer, Value::Number(Number::Integer(5)));
}

#[test]
fn the_last_element_is_the_one_a_path_cannot_reach() {
    let items = Value::Array(vec![text("a"), text("b"), text("c")]);
    assert_eq!(
        call(Function::ArrayLast, &[items], at()).expect("an element"),
        text("c")
    );
}

#[test]
fn an_empty_array_answers_with_an_absence_rather_than_a_failure() {
    // An array having no first element is a state, not a mistake — the same
    // answer a route reaching nothing gives.
    let empty = Value::Array(Vec::new());
    assert_eq!(
        call(Function::ArrayFirst, &[empty], at()).expect("an answer"),
        Value::None
    );
}

#[test]
fn an_absent_argument_answers_with_an_absence_rather_than_failing() {
    // Without this a single record missing a field would fail a whole read,
    // and a store holding documents of differing shapes cannot have a
    // function surface that only works when every record has every field.
    assert_eq!(
        call(Function::StringLen, &[Value::None], at()).expect("an answer"),
        Value::None
    );
    assert_eq!(
        call(Function::ArrayLen, &[Value::Null], at()).expect("an answer"),
        Value::None
    );
    // Except for the one function that asks about the value itself.
    assert_eq!(
        call(Function::TypeOf, &[Value::None], at()).expect("an answer"),
        text("none")
    );
}

#[test]
fn a_wrong_argument_names_the_function_the_position_and_both_types() {
    let error = call(
        Function::StringLen,
        &[Value::Number(Number::Integer(1))],
        at(),
    )
    .expect_err("a refusal");
    let message = error.to_string();
    assert!(message.contains("string::len"), "{message}");
    assert!(message.contains("argument 1"), "{message}");
    assert!(message.contains("string"), "{message}");
    assert!(message.contains("number"), "{message}");
}

#[test]
fn rounding_keeps_the_kind_it_was_given() {
    let whole = Value::Number(Number::Integer(7));
    same(
        call(Function::MathFloor, std::slice::from_ref(&whole), at()).expect("a number"),
        &whole,
    );
    let fractional = Value::Number(Number::float(2.5));
    same(
        call(Function::MathRound, &[fractional], at()).expect("a number"),
        Value::Number(Number::float(3.0)),
    );
}

/// The instant every `time::` test below reads.
fn instant(text: &str) -> Value {
    Value::Datetime(tessari_types::Datetime::parse_rfc3339(text).expect("an instant"))
}

fn read(function: Function, value: Value) -> Value {
    call(function, &[value], at()).expect("a reading")
}

#[test]
fn each_reading_takes_its_own_field_and_not_a_neighbours() {
    // The failure this guards is a copy-paste one: six arms differing by a
    // single field name, where `time::minute` returning the hour is not a
    // crash and not a type error. The fixture has six distinct values so no
    // two fields can be swapped without the test noticing.
    let taken = instant("2026-08-28T14:37:09Z");
    for (function, expected) in [
        (Function::TimeYear, 2026),
        (Function::TimeMonth, 8),
        (Function::TimeDay, 28),
        (Function::TimeHour, 14),
        (Function::TimeMinute, 37),
        (Function::TimeSecond, 9),
    ] {
        assert_eq!(
            format!("{:?}", read(function, taken.clone())),
            format!("{:?}", Value::Number(Number::Integer(expected))),
            "{function}"
        );
    }
}

#[test]
fn the_second_of_the_minute_is_not_the_second_since_the_epoch() {
    // Two functions one letter apart in meaning, and both answer an integer,
    // so nothing but this asserts which is which.
    let taken = instant("2026-08-28T14:37:09Z");
    same(
        read(Function::TimeSecond, taken.clone()),
        Value::Number(Number::Integer(9)),
    );
    same(
        read(Function::TimeUnix, taken),
        Value::Number(Number::Integer(1_787_927_829)),
    );
}

#[test]
fn an_instant_survives_a_round_trip_through_its_second_count() {
    let taken = instant("2026-08-28T14:37:09Z");
    let seconds = read(Function::TimeUnix, taken.clone());
    assert_eq!(read(Function::TimeFromUnix, seconds), taken);
}

#[test]
fn a_sub_second_remainder_is_dropped_by_the_second_count_and_not_refused() {
    // Deliberate, and the reason is in `time::unix`'s comment: there is no
    // `time::unix_millis` to write instead, and `time::now()` carries a
    // remainder nearly always. Asserted so the loss is a decision on the
    // record rather than something nobody looked at.
    let taken = instant("2026-08-28T14:37:09.5Z");
    same(
        read(Function::TimeUnix, taken),
        Value::Number(Number::Integer(1_787_927_829)),
    );
}

#[test]
fn a_fractional_second_count_is_refused_rather_than_truncated() {
    let error = call(
        Function::TimeFromUnix,
        &[Value::Number(Number::float(1.5))],
        at(),
    )
    .expect_err("a refusal");
    let message = error.to_string();
    assert!(message.contains("time::from_unix"), "{message}");
    assert!(message.contains("math::round"), "{message}");
    // A whole number written as a float is not a fraction, and is taken.
    assert_eq!(
        read(Function::TimeFromUnix, Value::Number(Number::float(0.0))),
        Value::Datetime(tessari_types::Datetime::from_seconds(0))
    );
}

#[test]
fn a_reading_of_an_absent_instant_is_an_absence_and_narrows_the_read() {
    // None of the eight join `answers_for_absence`: there is no year that a
    // missing field has. The general rule therefore applies, and a record
    // without the field drops out of the read instead of failing it.
    assert_eq!(
        call(Function::TimeYear, &[Value::None], at()).expect("an answer"),
        Value::None
    );
}
