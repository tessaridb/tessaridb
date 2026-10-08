//! Arithmetic over instants and spans (ADR-0124 D4).
//!
//! An instant moves by a span, two instants differ by one, and two spans add
//! and subtract. Nothing else is defined: an instant plus an instant names no
//! moment, and a span times a number is a scaling nobody has asked for yet.
//! A result past what the type holds is refused, never wrapped.

use tessari_ql::{ArithmeticOp, Span};
use tessari_types::Value;

use crate::error::{Error, Result};

/// The answer when both operands are times, `None` when either is not one.
pub(super) fn time(
    op: ArithmeticOp,
    left: &Value,
    right: &Value,
    span: Span,
) -> Option<Result<Value>> {
    let outside = |reason: &'static str| Error::ArithmeticFailed {
        operator: op.spelling(),
        reason,
        span,
    };
    let instant = "the result is outside the range an instant can hold";
    let length = "the result is outside the range a duration can hold";
    let answer = match (op, left, right) {
        (ArithmeticOp::Add, Value::Datetime(at), Value::Duration(by))
        | (ArithmeticOp::Add, Value::Duration(by), Value::Datetime(at)) => at
            .checked_add(*by)
            .map(Value::Datetime)
            .ok_or_else(|| outside(instant)),
        (ArithmeticOp::Subtract, Value::Datetime(at), Value::Duration(by)) => at
            .checked_sub(*by)
            .map(Value::Datetime)
            .ok_or_else(|| outside(instant)),
        (ArithmeticOp::Subtract, Value::Datetime(later), Value::Datetime(earlier)) => later
            .checked_since(*earlier)
            .map(Value::Duration)
            .ok_or_else(|| outside(length)),
        (ArithmeticOp::Add, Value::Duration(a), Value::Duration(b)) => a
            .checked_add(*b)
            .map(Value::Duration)
            .ok_or_else(|| outside(length)),
        (ArithmeticOp::Subtract, Value::Duration(a), Value::Duration(b)) => a
            .checked_sub(*b)
            .map(Value::Duration)
            .ok_or_else(|| outside(length)),
        _ => return None,
    };
    Some(answer)
}
