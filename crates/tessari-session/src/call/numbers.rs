//! Number shaping the functions share.

use super::arguments::{array_at, number_at};
use crate::error::{Error, Result};
use tessari_ql::{Aggregate, Function, Span};
use tessari_types::{Number, Value};

/// An element count as a value, refusing one no integer can hold.
pub(crate) fn count(size: usize, function: Function, span: Span) -> Result<Value> {
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
pub(crate) fn reshape(function: Function, number: &Number) -> Number {
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

/// One array folded by the accumulator the aggregates use.
///
/// The array is the group. Every value in it is offered in order, exactly as a
/// record's value is offered when the fold is over rows, so the promotion
/// rules, the treatment of an absence and the answer over nothing are not
/// restated here — they are the ones already written down and already tested.
pub(crate) fn folded(
    aggregate: Aggregate,
    function: Function,
    arguments: &[Value],
    span: Span,
) -> Result<Value> {
    let items = array_at(function, arguments, 0, span)?;
    let mut running = crate::accumulate::Accumulator::for_aggregate(aggregate, span);
    for value in items {
        running.offer(value)?;
    }
    running.finish()
}

/// A number's whole part, toward zero, keeping its kind.
///
/// Its own function rather than an arm of [`reshape`]: that one is the four
/// shapes `math::abs` and its neighbours give a number and its match is written
/// per kind, and adding a fifth there would grow a table whose whole point is
/// to be read at a glance.
pub(crate) fn truncated(number: &Number) -> Number {
    match number {
        Number::Integer(held) => Number::Integer(*held),
        Number::Decimal(held) => Number::Decimal(held.trunc()),
        Number::Float(held) => Number::float(held.trunc()),
    }
}

/// A base raised to an exponent.
///
/// **Two whole numbers answer a whole number**, which is the rule `math::abs`
/// and its neighbours already follow — a kind is kept where keeping it is
/// exact. `math::pow(2, 10)` is therefore `1024` and not `1024.0`. Anything
/// else — a fractional base, a fractional or negative exponent — answers a
/// float, since that is the only kind that holds the answer.
///
/// An integer result too large to hold is **refused rather than saturated**. A
/// saturated power is a wrong number that looks like a right one, and it would
/// be the largest number in the store, which is exactly the value most likely
/// to pass a sanity check unnoticed.
pub(crate) fn power(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let base = number_at(function, arguments, 0, span)?;
    let exponent = number_at(function, arguments, 1, span)?;
    let failed = |reason: &'static str| Error::CallFailed {
        function,
        reason,
        span,
    };
    if let (Some(base), Some(exponent)) = (base.as_exact_integer(), exponent.as_exact_integer())
        && exponent >= 0
    {
        let Ok(exponent) = u32::try_from(exponent) else {
            return Err(failed("that exponent is larger than any integer answer"));
        };
        return base
            .checked_pow(exponent)
            .map(|held| Value::Number(Number::Integer(held)))
            .ok_or_else(|| failed("that power is outside the integer range"));
    }
    let (Some(base), Some(exponent)) = (base.as_float(), exponent.as_float()) else {
        return Err(failed("that number is outside the range a float holds"));
    };
    let held = base.powf(exponent);
    if held.is_nan() || held.is_infinite() {
        return Err(failed("that power is not a number a float holds"));
    }
    Ok(Value::Number(Number::float(held)))
}

/// `math::min` and `math::max`.
pub(crate) fn extreme(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let first = number_at(function, arguments, 0, span)?;
    let second = number_at(function, arguments, 1, span)?;
    let smaller = function == Function::MathMin;
    let held = if (first <= second) == smaller {
        first
    } else {
        second
    };
    Ok(Value::Number(held.clone()))
}

/// `math::sign`.
pub(crate) fn sign(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let number = number_at(function, arguments, 0, span)?;
    let zero = Number::Integer(0);
    Ok(Value::Number(Number::Integer(match number.cmp(&zero) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    })))
}

/// `math::ln` and `math::exp`.
pub(crate) fn logarithm(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let number = number_at(function, arguments, 0, span)?;
    let Some(held) = number.as_float() else {
        return Err(Error::CallFailed {
            function,
            reason: "that number is outside the range a float holds",
            span,
        });
    };
    let answer = if function == Function::MathLn {
        if held <= 0.0_f64 {
            return Ok(Value::None);
        }
        held.ln()
    } else {
        held.exp()
    };
    if answer.is_finite() {
        Ok(Value::Number(Number::float(answer)))
    } else {
        Ok(Value::None)
    }
}

/// `math::sqrt`.
pub(crate) fn square_root(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let number = number_at(function, arguments, 0, span)?;
    let Some(held) = number.as_float() else {
        return Err(Error::CallFailed {
            function,
            reason: "that number is outside the range a float holds",
            span,
        });
    };
    // A negative root is refused rather than answered with a NaN. A NaN
    // compares false against everything including itself, so it would
    // travel through a filter and an ordering silently; `math::abs`
    // says what a caller who meant the magnitude should write.
    if held < 0.0 {
        return Err(Error::CallFailed {
            function,
            reason: "a square root of a negative number is not a number; math::abs says the magnitude",
            span,
        });
    }
    Ok(Value::Number(Number::float(held.sqrt())))
}
