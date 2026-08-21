//! Arithmetic over the three numeric kinds.
//!
//! # Kinds promote: int → decimal → float
//!
//! The result is the **wider** of the two operands, where wider means "can hold
//! what the other one holds". An integer converts to a decimal without loss, so
//! `int + decimal` is a decimal; a float can swallow either but only
//! approximately, so anything touching one is a float and says so.
//!
//! # Division always produces at least a decimal
//!
//! `7 / 2` is `3.5` and not `3`. Truncating integer division is the classic
//! silent wrong answer — the query looks right, the number is wrong, and nothing
//! raises — and it is the exact shape this store spends its rules refusing. The
//! cost is stated instead of hidden: `1 / 3` is a decimal rounded to the type's
//! precision, because no decimal holds a third.
//!
//! # Overflow and division by zero are failures, not values
//!
//! Integer arithmetic is checked, and a division by zero fails whatever the
//! kinds — including for floats, where the hardware would happily produce an
//! infinity. A wrapped integer or an infinity written into a record is a value
//! nobody meant, and by the time anyone notices it is stored.

use bgv_db_ql::{ArithmeticOp, Span};
use bgv_db_types::{Number, Value};
use rust_decimal::Decimal;

use crate::error::{Error, Result};

/// Apply an arithmetic operator to two values.
pub(crate) fn arithmetic(
    op: ArithmeticOp,
    left: &Value,
    right: &Value,
    span: Span,
) -> Result<Value> {
    let (Value::Number(left), Value::Number(right)) = (left, right) else {
        return Err(Error::NotArithmetic {
            operator: op.spelling(),
            left: left.type_name(),
            right: right.type_name(),
            span,
        });
    };
    let failed = |reason: &'static str| Error::ArithmeticFailed {
        operator: op.spelling(),
        reason,
        span,
    };

    // Division is the one operator that will not stay in the integers, so it
    // promotes before the kinds are consulted rather than after.
    let promoted = if matches!(op, ArithmeticOp::Divide) {
        Kind::of(left).max(Kind::of(right)).max(Kind::Decimal)
    } else {
        Kind::of(left).max(Kind::of(right))
    };

    match promoted {
        Kind::Integer => {
            let (Number::Integer(a), Number::Integer(b)) = (left, right) else {
                return Err(failed("the operands are not both integers"));
            };
            integer(op, *a, *b).map(|value| Value::Number(Number::Integer(value)))
        }
        Kind::Decimal => {
            let (Some(a), Some(b)) = (left.as_decimal(), right.as_decimal()) else {
                return Err(failed("a number is outside the exact range"));
            };
            decimal(op, a, b).map(|value| Value::Number(Number::Decimal(value)))
        }
        Kind::Float => {
            let (Some(a), Some(b)) = (as_float(left), as_float(right)) else {
                return Err(failed("a number is outside the range a float can hold"));
            };
            float(op, a, b).map(|value| Value::Number(Number::float(value)))
        }
    }
    .map_err(failed)
}

/// Negate a number, refusing anything else.
pub(crate) fn negate(value: &Value, span: Span) -> Result<Value> {
    let Value::Number(number) = value else {
        return Err(Error::NotArithmetic {
            operator: "-",
            left: value.type_name(),
            right: value.type_name(),
            span,
        });
    };
    let negated = match number {
        Number::Integer(held) => {
            let Some(negated) = held.checked_neg() else {
                return Err(Error::ArithmeticFailed {
                    operator: "-",
                    reason: "the result is outside the integer range",
                    span,
                });
            };
            Number::Integer(negated)
        }
        Number::Decimal(held) => Number::Decimal(held.saturating_mul(Decimal::NEGATIVE_ONE)),
        Number::Float(held) => Number::float(-*held),
    };
    Ok(Value::Number(negated))
}

/// How wide a numeric kind is: what it can hold that the narrower one cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Integer,
    Decimal,
    Float,
}

impl Kind {
    const fn of(number: &Number) -> Self {
        match number {
            Number::Integer(_) => Self::Integer,
            Number::Decimal(_) => Self::Decimal,
            Number::Float(_) => Self::Float,
        }
    }
}

/// A number as a float, when one can hold it.
fn as_float(number: &Number) -> Option<f64> {
    match number {
        Number::Float(held) => Some(*held),
        Number::Integer(held) => Some(f64::from(i32::try_from(*held).ok()?)),
        Number::Decimal(held) => f64::try_from(*held).ok(),
    }
}

fn integer(op: ArithmeticOp, a: i64, b: i64) -> core::result::Result<i64, &'static str> {
    let out_of_range = "the result is outside the integer range";
    match op {
        ArithmeticOp::Add => a.checked_add(b).ok_or(out_of_range),
        ArithmeticOp::Subtract => a.checked_sub(b).ok_or(out_of_range),
        ArithmeticOp::Multiply => a.checked_mul(b).ok_or(out_of_range),
        ArithmeticOp::Remainder => a.checked_rem(b).ok_or("a remainder by zero"),
        // Division promoted away from the integers before reaching here, so this
        // arm says what it would mean rather than claiming it cannot happen.
        ArithmeticOp::Divide => Err("a division that did not widen to a decimal"),
    }
}

fn decimal(
    op: ArithmeticOp,
    a: Decimal,
    b: Decimal,
) -> core::result::Result<Decimal, &'static str> {
    let out_of_range = "the result is outside the exact range";
    match op {
        ArithmeticOp::Add => a.checked_add(b).ok_or(out_of_range),
        ArithmeticOp::Subtract => a.checked_sub(b).ok_or(out_of_range),
        ArithmeticOp::Multiply => a.checked_mul(b).ok_or(out_of_range),
        ArithmeticOp::Divide => a.checked_div(b).ok_or("a division by zero"),
        ArithmeticOp::Remainder => a.checked_rem(b).ok_or("a remainder by zero"),
    }
}

fn float(op: ArithmeticOp, a: f64, b: f64) -> core::result::Result<f64, &'static str> {
    // A float division by zero would give an infinity rather than failing, and
    // an infinity stored in a record is a value nobody wrote. Refused here so
    // that every kind answers the same way.
    if matches!(op, ArithmeticOp::Divide | ArithmeticOp::Remainder) && b == 0.0 {
        return Err("a division by zero");
    }
    match op {
        ArithmeticOp::Add => Ok(a + b),
        ArithmeticOp::Subtract => Ok(a - b),
        ArithmeticOp::Multiply => Ok(a * b),
        ArithmeticOp::Divide => Ok(a / b),
        ArithmeticOp::Remainder => Ok(a % b),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_ql::{ArithmeticOp, Span};
    use bgv_db_types::{Number, Value};
    use rust_decimal::Decimal;

    use super::arithmetic;

    fn at() -> Span {
        Span::new(0, 1)
    }

    fn int(value: i64) -> Value {
        Value::Number(Number::Integer(value))
    }

    fn dec(value: &str) -> Value {
        Value::Number(Number::Decimal(
            value.parse::<Decimal>().expect("a decimal"),
        ))
    }

    #[test]
    fn integers_stay_integers_and_a_decimal_widens_them() {
        assert_eq!(
            arithmetic(ArithmeticOp::Add, &int(2), &int(3), at()).expect("a sum"),
            int(5)
        );
        assert_eq!(
            arithmetic(ArithmeticOp::Add, &int(2), &dec("0.5"), at()).expect("a sum"),
            dec("2.5")
        );
    }

    #[test]
    fn division_never_truncates() {
        // The whole reason `/` promotes before the kinds are consulted: `3` is
        // a plausible-looking wrong answer that nothing downstream would catch.
        assert_eq!(
            arithmetic(ArithmeticOp::Divide, &int(7), &int(2), at()).expect("a quotient"),
            dec("3.5")
        );
    }

    #[test]
    fn overflow_and_division_by_zero_are_failures_rather_than_values() {
        assert!(arithmetic(ArithmeticOp::Add, &int(i64::MAX), &int(1), at()).is_err());
        assert!(arithmetic(ArithmeticOp::Multiply, &int(i64::MIN), &int(-1), at()).is_err());
        assert!(arithmetic(ArithmeticOp::Divide, &int(1), &int(0), at()).is_err());
        // Including for floats, where the hardware would give an infinity.
        let one = Value::Number(Number::float(1.0));
        let zero = Value::Number(Number::float(0.0));
        assert!(arithmetic(ArithmeticOp::Divide, &one, &zero, at()).is_err());
    }

    #[test]
    fn arithmetic_on_something_that_is_not_a_number_names_both_types() {
        let error = arithmetic(ArithmeticOp::Add, &Value::from("ada"), &int(1), at())
            .expect_err("a refusal");
        let text = error.to_string();
        assert!(text.contains("string"), "{text}");
        assert!(text.contains("number"), "{text}");
    }
}
