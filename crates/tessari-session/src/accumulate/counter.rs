//! The counter folds — `increase`, `rate`, `delta` — over samples ordered by
//! their instant (ADR-0088 §5).
//!
//! # Exact over the samples, and nothing beyond them
//!
//! The answer is what the samples say: no extrapolation to the edges of a
//! window, which is where this differs from Prometheus and why the docs say so
//! beside the function. A fall between two samples is a counter reset, and the
//! sample after it counts from zero. Integers and decimals are summed exactly,
//! as `sum` does; a single float sample turns the fold to floats, as it does
//! there too.

use rust_decimal::Decimal;
use tessari_ql::{Aggregate, Span};
use tessari_types::{Datetime, Number, Value};

use super::failed;
use crate::aggregate::approximate;
use crate::error::Result;

/// Nanoseconds in a second, as a divisor.
const NANOS: i64 = 1_000_000_000;

/// The fold's answer over `held`, which arrives in record order.
///
/// # Errors
///
/// [`crate::error::Error::NotSummable`] when exact arithmetic leaves its range.
pub(crate) fn finish(fold: Aggregate, held: &[(Datetime, Number)], span: Span) -> Result<Value> {
    let mut samples = held.to_vec();
    samples.sort_by_key(|(at, _)| *at);
    let (Some((first_at, _)), Some((last_at, _))) = (samples.first(), samples.last()) else {
        return Ok(Value::None);
    };
    if samples.len() < 2 {
        return Ok(Value::None);
    }
    let seconds = elapsed(*first_at, *last_at);
    let name = fold.spelling();
    if samples
        .iter()
        .any(|(_, value)| matches!(value, Number::Float(_)))
    {
        let values: Vec<f64> = samples
            .iter()
            .map(|(_, value)| approximate(value).unwrap_or(f64::NAN))
            .collect();
        let answer = match fold {
            Aggregate::Delta => values.last().zip(values.first()).map(|(b, a)| b - a),
            Aggregate::Rate => {
                let per = seconds.and_then(|held| approximate(&Number::Decimal(held)));
                per.filter(|per| *per > 0.0)
                    .map(|per| rises_float(&values) / per)
            }
            _ => Some(rises_float(&values)),
        };
        return Ok(answer.map_or(Value::None, |held| Value::Number(Number::float(held))));
    }
    let mut values = Vec::with_capacity(samples.len());
    for (_, value) in &samples {
        values.push(
            value
                .as_decimal()
                .ok_or_else(|| failed(name, "a number outside the exact range", span))?,
        );
    }
    let integral = samples
        .iter()
        .all(|(_, value)| matches!(value, Number::Integer(_)));
    let answer = match fold {
        Aggregate::Delta => values
            .last()
            .zip(values.first())
            .and_then(|(b, a)| b.checked_sub(*a))
            .ok_or_else(|| failed(name, "a difference outside the exact range", span))?,
        Aggregate::Rate => {
            let Some(per) = seconds.filter(|per| *per > Decimal::ZERO) else {
                return Ok(Value::None);
            };
            return rises_exact(&values)
                .and_then(|rise| rise.checked_div(per))
                .map_or_else(
                    || Err(failed(name, "a rate outside the exact range", span)),
                    |rate| Ok(Value::Number(Number::Decimal(rate.normalize()))),
                );
        }
        _ => rises_exact(&values)
            .ok_or_else(|| failed(name, "a total outside the exact range", span))?,
    };
    if integral && let Ok(whole) = i64::try_from(answer) {
        return Ok(Value::Number(Number::Integer(whole)));
    }
    Ok(Value::Number(Number::Decimal(answer.normalize())))
}

/// The seconds between two instants, exactly.
fn elapsed(first: Datetime, last: Datetime) -> Option<Decimal> {
    let whole = last.seconds().checked_sub(first.seconds())?;
    let nanos = i64::from(last.nanos()).checked_sub(i64::from(first.nanos()))?;
    let total = i128::from(whole)
        .checked_mul(i128::from(NANOS))?
        .checked_add(i128::from(nanos))?;
    Decimal::from_i128_with_scale(total, 9).into()
}

/// The sum of the rises, a fall counting as a reset to zero.
fn rises_exact(values: &[Decimal]) -> Option<Decimal> {
    let mut total = Decimal::ZERO;
    for pair in values.windows(2) {
        let [before, after] = pair else { continue };
        let rise = if after >= before {
            after.checked_sub(*before)?
        } else {
            *after
        };
        total = total.checked_add(rise)?;
    }
    Some(total)
}

/// [`rises_exact`] over floats.
fn rises_float(values: &[f64]) -> f64 {
    values
        .windows(2)
        .map(|pair| match pair {
            [before, after] if after >= before => after - before,
            [_, after] => *after,
            _ => 0.0,
        })
        .sum()
}
