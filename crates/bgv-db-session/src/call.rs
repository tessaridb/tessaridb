//! What each of the language's functions does, once its arguments are values.
//!
//! Arity was checked when the statement was read (`bgv-db-ql`'s `Function`
//! knows the set), so what is left here is what each argument *holds* — which
//! nothing could know before a record was in hand.
//!
//! A wrong type names the function, the position, what was wanted and what was
//! there. A message saying only "wrong type" makes the author guess which of
//! three arguments it meant.

use bgv_db_ql::{Function, Span};
use bgv_db_types::{Datetime, Number, Value};

use crate::error::{Error, Result};

/// Evaluate a call, with its arguments already values.
///
/// **An absent or null argument answers `none`**, without the function being
/// run. A route that reaches nothing evaluates to `none`, so without this rule a
/// single record missing a field would fail the whole read — and a store that
/// holds documents of differing shapes cannot have a function surface that only
/// works when every record has every field. It is the same rule a filter
/// already applies one level up, carried into the calls.
///
/// [`Function::TypeOf`] is the one exception, and it is the exception that shows
/// the rule: it is the only function asking *about* the value rather than
/// computing from it, so an absence is its answer rather than its obstacle.
pub(crate) fn call(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    if function != Function::TypeOf
        && arguments
            .iter()
            .any(|value| !value.is_present() || *value == Value::Null)
    {
        return Ok(Value::None);
    }
    match function {
        Function::StringLen => {
            let text = text_at(function, arguments, 0, span)?;
            // Characters, not bytes: a caller asking how long a name is means
            // the name, not its encoding.
            count(text.chars().count(), function, span)
        }
        Function::StringLower => Ok(Value::from(
            text_at(function, arguments, 0, span)?
                .to_lowercase()
                .as_str(),
        )),
        Function::StringUpper => Ok(Value::from(
            text_at(function, arguments, 0, span)?
                .to_uppercase()
                .as_str(),
        )),
        Function::StringTrim => Ok(Value::from(text_at(function, arguments, 0, span)?.trim())),
        Function::StringConcat => {
            let first = text_at(function, arguments, 0, span)?;
            let second = text_at(function, arguments, 1, span)?;
            Ok(Value::from(format!("{first}{second}").as_str()))
        }
        Function::ArrayLen => {
            let items = array_at(function, arguments, 0, span)?;
            count(items.len(), function, span)
        }
        Function::ArrayFirst => Ok(array_at(function, arguments, 0, span)?
            .first()
            .cloned()
            .unwrap_or(Value::None)),
        Function::ArrayLast => Ok(array_at(function, arguments, 0, span)?
            .last()
            .cloned()
            .unwrap_or(Value::None)),
        Function::MathAbs | Function::MathFloor | Function::MathCeil | Function::MathRound => {
            let number = number_at(function, arguments, 0, span)?;
            Ok(Value::Number(reshape(function, number)))
        }
        // Evaluated in the session, so the instant that reaches the log is a
        // value like any other — a replica applies what was written rather than
        // asking its own clock and reaching a different answer.
        Function::TimeNow => now(function, span),
        Function::TypeOf => {
            let Some(value) = arguments.first() else {
                return Err(wrong_type(function, 0, "a value", "nothing", span));
            };
            Ok(Value::from(value.type_name()))
        }
    }
}

/// The instant this statement is being evaluated at.
///
/// Read once, in the session, so the value that reaches the log is a value like
/// any other — a replica applies what was written rather than asking its own
/// clock and reaching a different answer. A clock before the epoch is refused
/// rather than folded to zero: a machine whose time is wrong should say so.
fn now(function: Function, span: Span) -> Result<Value> {
    let failed = |reason: &'static str| Error::CallFailed {
        function,
        reason,
        span,
    };
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| failed("the clock is before the epoch"))?;
    let seconds = i64::try_from(since.as_secs()).map_err(|_| failed("the clock is unreadable"))?;
    let instant = Datetime::new(seconds, since.subsec_nanos())
        .ok_or_else(|| failed("the clock is unreadable"))?;
    Ok(Value::Datetime(instant))
}

/// An element count as a value, refusing one no integer can hold.
fn count(size: usize, function: Function, span: Span) -> Result<Value> {
    let Ok(size) = i64::try_from(size) else {
        return Err(Error::CallFailed {
            function,
            reason: "the count is outside the integer range",
            span,
        });
    };
    Ok(Value::Number(Number::Integer(size)))
}

/// The four shapes `math::*` gives a number, each keeping its kind.
fn reshape(function: Function, number: &Number) -> Number {
    match (function, number) {
        (Function::MathAbs, Number::Integer(held)) => Number::Integer(held.saturating_abs()),
        (Function::MathAbs, Number::Decimal(held)) => Number::Decimal(held.abs()),
        (Function::MathAbs, Number::Float(held)) => Number::float(held.abs()),
        (Function::MathFloor, Number::Decimal(held)) => Number::Decimal(held.floor()),
        (Function::MathFloor, Number::Float(held)) => Number::float(held.floor()),
        (Function::MathCeil, Number::Decimal(held)) => Number::Decimal(held.ceil()),
        (Function::MathCeil, Number::Float(held)) => Number::float(held.ceil()),
        (Function::MathRound, Number::Decimal(held)) => Number::Decimal(
            held.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero),
        ),
        (Function::MathRound, Number::Float(held)) => Number::float(held.round()),
        // An integer is already whole, so flooring, ceiling and rounding one is
        // the number itself rather than a conversion nobody asked for.
        (_, held) => held.clone(),
    }
}

fn text_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&str> {
    match arguments.get(at) {
        Some(Value::String(text)) => Ok(text),
        other => Err(wrong_type(function, at, "a string", named(other), span)),
    }
}

fn array_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&[Value]> {
    match arguments.get(at) {
        Some(Value::Array(items)) => Ok(items),
        other => Err(wrong_type(function, at, "an array", named(other), span)),
    }
}

fn number_at(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<&Number> {
    match arguments.get(at) {
        Some(Value::Number(number)) => Ok(number),
        other => Err(wrong_type(function, at, "a number", named(other), span)),
    }
}

fn named(value: Option<&Value>) -> &'static str {
    value.map_or("nothing", Value::type_name)
}

fn wrong_type(
    function: Function,
    at: usize,
    expected: &'static str,
    found: &'static str,
    span: Span,
) -> Error {
    Error::WrongArgument {
        function,
        at: at.saturating_add(1),
        expected,
        found,
        span,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_ql::{Function, Span};
    use bgv_db_types::{Number, Value};

    use super::call;

    fn at() -> Span {
        Span::new(0, 1)
    }

    fn text(value: &str) -> Value {
        Value::from(value)
    }

    #[test]
    fn a_length_counts_characters_and_not_bytes() {
        let answer = call(Function::StringLen, &[text("héllo")], at()).expect("a length");
        assert_eq!(answer, Value::Number(Number::Integer(5)));
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
        assert_eq!(
            call(Function::MathFloor, std::slice::from_ref(&whole), at()).expect("a number"),
            whole
        );
        let fractional = Value::Number(Number::float(2.5));
        assert_eq!(
            call(Function::MathRound, &[fractional], at()).expect("a number"),
            Value::Number(Number::float(3.0))
        );
    }
}
