//! Turning a value into a kind the caller names.
//!
//! # A cast is an assertion, not a projection
//!
//! Every arm below either produces the value the caller asked for or refuses.
//! None of them produce something *near* it. That is the single rule the module
//! is built on, and it is worth stating because the alternative is so easy to
//! write: `type::int(2.5)` could truncate, `type::bool(1)` could say `true`,
//! `type::string([1, 2])` could render `<array of 2>`, and every one of those
//! returns a value the caller cannot tell from a correct one.
//!
//! The store already refuses in exactly this shape elsewhere — a read past its
//! ceiling, a join whose keys differ, an unbounded read standing in an
//! expression. Each answers with a refusal rather than with a number that looks
//! like an answer. A cast belongs in that company.
//!
//! # What a refusal is not
//!
//! An **absent** argument is not a refusal: it answers `none` before this module
//! is reached (`call`'s rule, and `Function::answers_for_absence` decides who is
//! exempt). A field that is not there is a shape, not a mistake, and a store
//! holding documents of differing shapes cannot have casts that only work when
//! every record has every field.
//!
//! So there are three outcomes and they mean three different things: `none` for
//! a field that was not there, the value for one that converts, and a refusal
//! naming the value for one that does not.
//!
//! # Why the conversions between number kinds are exact
//!
//! `math::floor`, `math::ceil` and `math::round` already say which whole number
//! a fractional one was meant to become. A cast that picked one of the three
//! would be answering a question the caller did not ask, in a function whose
//! name promises only a change of kind. So `type::int(2.5)` refuses and
//! `type::int(2.0)` does not.
//!
//! `type::float` is **not** symmetric with that, and the asymmetry is the
//! interesting part. A decimal takes its nearest float, because `19.99` has no
//! exact float and a rule demanding one would refuse nearly every decimal
//! anybody holds — leaving no way to say the conversion at all, where
//! `type::int` always has `math::round` beside it. An integer past 2^53 still
//! refuses, because there the nearest float is a *different integer*, and an
//! integer here is a count or an identity.
//!
//! Both rules live on `Number` — `as_exact_integer` and `as_float` — because
//! that is where the comparison they must agree with lives.

use tessari_ql::{Function, Span};
use tessari_types::{Datetime, Number, Value, parse_uuid, uuid_to_text};

use crate::error::{Error, Result};

/// Read `value` as the kind `function` names.
///
/// The caller has already applied the absent-argument rule, so `value` is
/// present here.
pub(crate) fn read(function: Function, value: &Value, span: Span) -> Result<Value> {
    let converted = match function {
        Function::TypeBool => boolean(value),
        Function::TypeInt => integer(value),
        Function::TypeFloat => float(value),
        Function::TypeString => text(value),
        Function::TypeDatetime => instant(value),
        Function::TypeUuid => identifier(value),
        // Unreachable through `call`, which routes only the six. Answering with
        // a refusal rather than a panic keeps that true even if a seventh cast
        // is added and its arm forgotten: the query fails, saying which cast has
        // no reading, instead of taking the process down.
        _ => None,
    };
    converted.ok_or_else(|| Error::NotCastable {
        function,
        value: value.to_string(),
        target: target(function),
        span,
    })
}

/// The kind a cast names, for the refusal's message.
fn target(function: Function) -> &'static str {
    // The spelling is the kind, which is the property
    // `every_cast_spells_its_kind_the_way_a_field_declaration_does` holds in
    // place — so the message names the kind without a second table to keep in
    // step with the first.
    function
        .spelling()
        .strip_prefix("type::")
        .unwrap_or_else(|| function.spelling())
}

/// A boolean, from a boolean or from the two words that spell one.
///
/// A number is **not** accepted. `1` meaning true is a convention from languages
/// with no boolean, and importing it here would make `type::bool(count)` answer
/// `true` for every count but zero — which is a question `count != 0` asks
/// clearly and this function would answer by accident.
fn boolean(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(held) => Some(Value::Bool(*held)),
        // Exactly the two spellings the language writes, and no others: `'yes'`,
        // `'Y'` and `'1'` are guesses about what somebody meant, and a guess
        // that is right nine times in ten is worse than a refusal.
        Value::String(held) if held == "true" => Some(Value::Bool(true)),
        Value::String(held) if held == "false" => Some(Value::Bool(false)),
        _ => None,
    }
}

/// An integer, from a whole number or from text spelling one.
fn integer(value: &Value) -> Option<Value> {
    match value {
        Value::Number(number) => number.as_exact_integer().map(Value::from),
        Value::String(held) => held.trim().parse::<i64>().ok().map(Value::from),
        _ => None,
    }
}

/// A float, from a number a float can stand for or from text spelling one.
fn float(value: &Value) -> Option<Value> {
    match value {
        Value::Number(number) => number
            .as_float()
            .map(|held| Value::Number(Number::float(held))),
        Value::String(held) => held
            .trim()
            .parse::<f64>()
            .ok()
            .map(|held| Value::Number(Number::float(held))),
        _ => None,
    }
}

/// The text a value reads back from.
///
/// Only the kinds that **have** such text. A `Display` exists for every value,
/// but for an array it writes `<array of 2>` and for bytes `<12 bytes>` — those
/// are renderings for a person reading a log, and a cast's answer is a value
/// that will be stored and read again. Producing one of them here would put a
/// sentence about a value where the value belonged, and nothing downstream could
/// tell the difference.
///
/// So a shape, a record reference, a pattern, bytes and the three collections
/// refuse. What they need is a serialisation with a reader on the other side,
/// which is a different thing from a cast and is not this function.
fn text(value: &Value) -> Option<Value> {
    match value {
        Value::String(held) => Some(Value::from(held.as_str())),
        Value::Bool(held) => Some(Value::from(if *held { "true" } else { "false" })),
        Value::Number(held) => Some(Value::from(held.to_string().as_str())),
        Value::Datetime(held) => Some(Value::from(held.to_rfc3339().as_str())),
        Value::Duration(held) => Some(Value::from(held.to_literal().as_str())),
        Value::Uuid(held) => Some(Value::from(uuid_to_text(held).as_str())),
        _ => None,
    }
}

/// An instant, from an instant or from RFC 3339 text.
///
/// A number is not accepted, and that is a refusal rather than a gap. Seconds
/// since the epoch is one reading of those digits and "the year 1700000000" is
/// another, so a cast choosing between them silently would be answering a
/// question the caller never settled. Naming which reading is meant is a
/// separate function's job, and it does not exist yet.
fn instant(value: &Value) -> Option<Value> {
    match value {
        Value::Datetime(held) => Some(Value::Datetime(*held)),
        Value::String(held) => Datetime::parse_rfc3339(held).map(Value::Datetime),
        _ => None,
    }
}

/// A UUID, from a UUID or from either of its two written forms.
fn identifier(value: &Value) -> Option<Value> {
    match value {
        Value::Uuid(held) => Some(Value::Uuid(*held)),
        Value::String(held) => parse_uuid(held).map(Value::Uuid),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use tessari_ql::{Function, Span};
    use tessari_types::{Number, Value};

    use super::read;

    fn at() -> Span {
        Span::new(0, 1)
    }

    fn cast(function: Function, value: Value) -> Option<Value> {
        read(function, &value, at()).ok()
    }

    /// Compare two answers by their written form rather than by `PartialEq`.
    ///
    /// `Value`'s equality equates `Integer(3)`, `Float(3.0)` and
    /// `Decimal("3.0")` deliberately, and the join relies on it. This module is
    /// the one place where the kind IS the answer, so an assertion written with
    /// `assert_eq!` on the values themselves cannot see what `type::int` is for:
    /// it passes against a cast that answered the other kind, and against one
    /// that handed the value straight back (Q-76).
    #[track_caller]
    fn same(answered: impl core::fmt::Debug, expected: impl core::fmt::Debug) {
        assert_eq!(format!("{answered:?}"), format!("{expected:?}"));
    }

    #[test]
    fn text_becomes_the_kind_it_spells() {
        same(
            cast(Function::TypeInt, Value::from("42")),
            Some(Value::from(42_i64)),
        );
        assert_eq!(
            cast(Function::TypeBool, Value::from("true")),
            Some(Value::Bool(true))
        );
        // Left on `assert_eq!` deliberately: `float` answers through
        // `Number::float` and nothing else, so the only wrong kind it could
        // give back is the String it was handed, which equality already sees.
        assert_eq!(
            cast(Function::TypeFloat, Value::from("2.5")),
            Some(Value::Number(Number::float(2.5)))
        );
        assert!(cast(Function::TypeDatetime, Value::from("2026-01-01T00:00:00Z")).is_some());
        assert!(
            cast(
                Function::TypeUuid,
                Value::from("550e8400-e29b-41d4-a716-446655440000")
            )
            .is_some()
        );
    }

    #[test]
    fn a_value_the_kind_cannot_hold_is_refused_and_not_answered_with_an_absence() {
        // The decision this whole module rests on. `none` would say "the field
        // was not there", which is a different fact and the one thing the value
        // system is most careful to keep separate.
        let error = read(Function::TypeInt, &Value::from("ada"), at()).expect_err("a refusal");
        let message = error.to_string();
        assert!(message.contains("type::int"), "{message}");
        assert!(message.contains("ada"), "{message}");
        assert!(message.contains("int"), "{message}");
    }

    #[test]
    fn a_fraction_is_refused_rather_than_truncated() {
        // `math::floor`, `math::ceil` and `math::round` say which whole number
        // was meant; this function would be picking one of them silently.
        assert_eq!(
            cast(Function::TypeInt, Value::Number(Number::float(2.5))),
            None
        );
        same(
            cast(Function::TypeInt, Value::Number(Number::float(2.0))),
            Some(Value::from(2_i64)),
        );
    }

    #[test]
    fn a_number_is_not_a_boolean_and_a_boolean_is_not_a_number() {
        assert_eq!(cast(Function::TypeBool, Value::from(1_i64)), None);
        assert_eq!(cast(Function::TypeBool, Value::from(0_i64)), None);
        assert_eq!(cast(Function::TypeInt, Value::Bool(true)), None);
        // And the near-spellings of a boolean are guesses, not values.
        for guess in ["yes", "TRUE", "True", "1", ""] {
            assert_eq!(
                cast(Function::TypeBool, Value::from(guess)),
                None,
                "{guess}"
            );
        }
    }

    #[test]
    fn text_is_produced_only_where_it_reads_back() {
        assert_eq!(
            cast(Function::TypeString, Value::from(7_i64)),
            Some(Value::from("7"))
        );
        assert_eq!(
            cast(Function::TypeString, Value::Bool(false)),
            Some(Value::from("false"))
        );
        // A rendering is not a value: `<array of 2>` would be a sentence about
        // an array sitting where the array's text belonged.
        assert_eq!(
            cast(
                Function::TypeString,
                Value::Array(vec![Value::from(1_i64), Value::from(2_i64)])
            ),
            None
        );
        assert_eq!(cast(Function::TypeString, Value::Bytes(vec![1, 2])), None);
    }

    #[test]
    fn an_instant_and_a_uuid_survive_a_round_trip_through_text() {
        // The property that makes `type::string` worth having: what it writes,
        // the matching cast reads back to the value it started from.
        let instant = cast(
            Function::TypeDatetime,
            Value::from("2026-08-28T12:30:15.5Z"),
        )
        .expect("an instant");
        let written = cast(Function::TypeString, instant.clone()).expect("its text");
        assert_eq!(cast(Function::TypeDatetime, written), Some(instant));

        let identifier = cast(
            Function::TypeUuid,
            Value::from("550e8400-e29b-41d4-a716-446655440000"),
        )
        .expect("a uuid");
        let written = cast(Function::TypeString, identifier.clone()).expect("its text");
        assert_eq!(written, Value::from("550e8400-e29b-41d4-a716-446655440000"));
        assert_eq!(cast(Function::TypeUuid, written), Some(identifier));
    }
}
