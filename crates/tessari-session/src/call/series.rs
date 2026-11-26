//! The `series::` functions — readings of a whole numeric series held as one
//! array, the shapes a chart or an alert asks about.

use super::arguments::{array_at, number_at};
use crate::error::{Error, Result};
use tessari_ql::{Function, Span};
use tessari_types::{Number, Value};

/// The series as floats; a value that is not a number refuses by name, the
/// way every numeric reading here refuses one.
fn floats(function: Function, arguments: &[Value], span: Span) -> Result<Vec<f64>> {
    let held = array_at(function, arguments, 0, span)?;
    let mut series = Vec::with_capacity(held.len());
    for value in held {
        match value {
            Value::Number(number) => {
                series.push(crate::aggregate::approximate(number).unwrap_or(f64::NAN))
            }
            _ => {
                return Err(Error::CallFailed {
                    function,
                    reason: "a series is an array of numbers",
                    span,
                });
            }
        }
    }
    Ok(series)
}

/// The indices outside the Tukey fences: below `Q1 − 1.5·IQR` or above
/// `Q3 + 1.5·IQR`, with the quartiles read by linear interpolation between
/// the closest ranks — the reading a spreadsheet makes.
pub(crate) fn outliers_tukey(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let series = floats(function, arguments, span)?;
    if series.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let mut sorted = series.clone();
    sorted.sort_by(f64::total_cmp);
    let last = sorted.len().saturating_sub(1);
    let quartile = |rank: f64| -> f64 {
        let place = rank * wide(last);
        let low = place.floor();
        let (Some(low), Some(high)) = (
            rust_decimal::prelude::ToPrimitive::to_usize(&low),
            rust_decimal::prelude::ToPrimitive::to_usize(&place.ceil()),
        ) else {
            return 0.0;
        };
        let part = place - wide(low);
        sorted[low.min(last)] * (1.0 - part) + sorted[high.min(last)] * part
    };
    let (first, third) = (quartile(0.25), quartile(0.75));
    let reach = (third - first) * 1.5;
    let (floor, ceiling) = (first - reach, third + reach);
    Ok(Value::Array(
        series
            .iter()
            .enumerate()
            .filter(|(_, held)| **held < floor || **held > ceiling)
            .map(|(place, _)| {
                Value::Number(Number::Integer(i64::try_from(place).unwrap_or(i64::MAX)))
            })
            .collect(),
    ))
}

/// The dominant period in samples: the strongest non-DC component of the
/// discrete Fourier transform, read as `n / k`. A direct transform, which is
/// `n²` — honest for the series lengths a record holds, and said here rather
/// than discovered.
pub(crate) fn period_detect(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let series = floats(function, arguments, span)?;
    if series.len() < 4 {
        return Ok(Value::None);
    }
    let count = wide(series.len());
    let mut best = (0_usize, 0.0_f64);
    for harmonic in 1..=series.len().checked_div(2).unwrap_or(0) {
        let k = wide(harmonic);
        let mut re = 0.0;
        let mut im = 0.0;
        for (place, held) in series.iter().enumerate() {
            let angle = -2.0 * std::f64::consts::PI * k * wide(place) / count;
            re += held * angle.cos();
            im += held * angle.sin();
        }
        let power = re * re + im * im;
        if power > best.1 {
            best = (harmonic, power);
        }
    }
    if best.0 == 0 {
        return Ok(Value::None);
    }
    Ok(Value::Number(Number::float(count / wide(best.0))))
}

/// Classical seasonal decomposition: the trend a centred moving average of
/// one period wide reads, the seasonal part the per-phase mean of what is
/// left (centred so its phases sum to nothing), and the residual. Named for
/// what it is in the doc — a LOESS STL this is not, and the spelling says
/// `stl` because that is the word the reader brings.
pub(crate) fn decompose_stl(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let series = floats(function, arguments, span)?;
    let period = number_at(function, arguments, 1, span)?;
    let Some(period) = crate::aggregate::approximate(period) else {
        return Err(Error::CallFailed {
            function,
            reason: "a period in whole samples",
            span,
        });
    };
    if period.fract() != 0.0 || period < 2.0 {
        return Err(Error::CallFailed {
            function,
            reason: "a period of at least two whole samples",
            span,
        });
    }
    let Some(period) = rust_decimal::prelude::ToPrimitive::to_usize(&period) else {
        return Err(Error::CallFailed {
            function,
            reason: "a period of at least two whole samples",
            span,
        });
    };
    let count = series.len();
    // The centred moving average; an even period centres between two places,
    // which the half-step form below reads the same way for both parities.
    let mut trend = vec![None::<f64>; count];
    let half = period.checked_div(2).unwrap_or(0);
    for (place, slot) in trend.iter_mut().enumerate() {
        if place < half || place.saturating_add(period.saturating_sub(half)) > count {
            continue;
        }
        let start = place.saturating_sub(half);
        let window = &series[start..start.saturating_add(period)];
        *slot = Some(window.iter().sum::<f64>() / wide(period));
    }
    // The per-phase mean of the detrended values, centred to sum to nothing.
    let mut seasonal = vec![0.0_f64; period];
    let mut seen = vec![0_usize; period];
    for (place, held) in series.iter().enumerate() {
        if let Some(middle) = trend[place] {
            let phase = place.checked_rem(period).unwrap_or(0);
            seasonal[phase] += held - middle;
            seen[phase] = seen[phase].saturating_add(1);
        }
    }
    for (phase, total) in seasonal.iter_mut().enumerate() {
        if seen[phase] > 0 {
            *total /= wide(seen[phase]);
        }
    }
    let mean: f64 = seasonal.iter().sum::<f64>() / wide(period);
    for phase in &mut seasonal {
        *phase -= mean;
    }
    let number =
        |held: Option<f64>| held.map_or(Value::None, |held| Value::Number(Number::float(held)));
    let trend: Vec<Value> = trend.into_iter().map(number).collect();
    let seasonal_answer: Vec<Value> = (0..count)
        .map(|place| {
            Value::Number(Number::float(
                seasonal[place.checked_rem(period).unwrap_or(0)],
            ))
        })
        .collect();
    let residual: Vec<Value> = series
        .iter()
        .enumerate()
        .map(|(place, held)| match trend_at(&trend, place) {
            Some(middle) => Value::Number(Number::float(
                *held - middle - seasonal[place.checked_rem(period).unwrap_or(0)],
            )),
            None => Value::None,
        })
        .collect();
    let mut parts = std::collections::BTreeMap::new();
    parts.insert("residual".to_string(), Value::Array(residual));
    parts.insert("seasonal".to_string(), Value::Array(seasonal_answer));
    parts.insert("trend".to_string(), Value::Array(trend));
    Ok(Value::Object(parts))
}

/// The trend's value at a place, back out of the answer array.
fn trend_at(trend: &[Value], place: usize) -> Option<f64> {
    match trend.get(place) {
        Some(Value::Number(held)) => crate::aggregate::approximate(held),
        _ => None,
    }
}

/// A place count as the float the arithmetic runs in.
fn wide(held: usize) -> f64 {
    crate::accumulate::count(u64::try_from(held).unwrap_or(u64::MAX))
}
