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
//! there too, and the float rises are summed exactly and rounded once
//! (ADR-0121 D4), so the answer does not depend on how the samples were split.
//!
//! # In parts
//!
//! Everything the answer needs from a run of samples consecutive in time is a
//! [`Summary`]: how many, the first and the last, and the rises between them.
//! Two runs that follow one another join into the summary of both — the rise
//! across the seam is the one sample pair the parts did not see. A leader sends
//! its shard's summary; when two shards' samples interleave in time the
//! summaries cannot be put end to end and the read asks for the samples
//! instead (ADR-0121 D3).

use rust_decimal::Decimal;
use tessari_ql::{Aggregate, Span};
use tessari_types::{Datetime, Number, Value};

use super::exact::ExactSum;
use super::{Running, failed};
use crate::aggregate::approximate;
use crate::error::Result;

/// Nanoseconds in a second, as a divisor.
const NANOS: i64 = 1_000_000_000;

/// Why an exact form cannot answer: a sample no decimal holds.
pub(crate) const INEXACT: &str = "a number outside the exact range";

/// One sample: when, and what the counter read.
pub(crate) type Sample = (Datetime, Number);

/// What one part of a group contributed, in key order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Part {
    /// Samples held here or sent as they are.
    Samples(Vec<Sample>),
    /// A leader's summary of its shard.
    Summary(Summary),
}

/// A run of samples consecutive in time, as much of it as the answer needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Summary {
    /// How many samples.
    pub(crate) counted: u64,
    /// The earliest, the first of equals in key order.
    pub(crate) first: Sample,
    /// The latest, the last of equals in key order.
    pub(crate) last: Sample,
    /// The rises exactly, or why that form cannot answer.
    pub(crate) exact: Running<Decimal>,
    /// The rises of the samples taken as floats, each rounded, summed exactly.
    pub(crate) float: ExactSum,
    /// Whether a sample was a float.
    pub(crate) saw_float: bool,
    /// Whether every sample was an integer.
    pub(crate) all_integer: bool,
    /// How many falls between consecutive samples the run saw — the counter
    /// resets (`resets`, ADR-0088 §5's fall rule read as the answer itself).
    pub(crate) resets: u64,
    /// The least-squares sums, `x` in seconds since `first`'s instant
    /// (`deriv` / `predict_linear`). Floats by declaration, like every
    /// regression; they merge by origin-shifting, so parts answer what the
    /// walk answers.
    pub(crate) regression: Regression,
}

/// The four sums a least-squares line needs, over `x` relative to the run's
/// first instant. `n` is the summary's own `counted`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Regression {
    /// Σx.
    pub(crate) sum_x: f64,
    /// Σx².
    pub(crate) sum_xx: f64,
    /// Σy.
    pub(crate) sum_y: f64,
    /// Σxy.
    pub(crate) sum_xy: f64,
}

impl Summary {
    /// The run of one sample.
    fn single(sample: Sample) -> Self {
        Self {
            counted: 1,
            exact: sample.1.as_decimal().map(|_| Decimal::ZERO).ok_or(INEXACT),
            float: ExactSum::default(),
            saw_float: matches!(sample.1, Number::Float(_)),
            all_integer: matches!(sample.1, Number::Integer(_)),
            resets: 0,
            regression: Regression {
                sum_x: 0.0,
                sum_xx: 0.0,
                sum_y: approximate(&sample.1).unwrap_or(f64::NAN),
                sum_xy: 0.0,
            },
            first: sample.clone(),
            last: sample,
        }
    }

    /// The summary of `samples`, which are sorted here by instant, equals
    /// keeping their order; `None` for no samples.
    pub(crate) fn of(samples: &[Sample]) -> Option<Self> {
        let mut sorted = samples.to_vec();
        sorted.sort_by_key(|(at, _)| *at);
        let mut held = sorted.into_iter();
        let mut summary = Self::single(held.next()?);
        for sample in held {
            summary.then(&Self::single(sample));
        }
        Some(summary)
    }

    /// Join `next`, whose samples all follow this run's, onto its end.
    pub(crate) fn then(&mut self, next: &Self) {
        let (before, after) = (&self.last.1, &next.first.1);
        self.exact = match (&self.exact, &next.exact) {
            (Ok(held), Ok(more)) => bridge_exact(before, after)
                .and_then(|rise| held.checked_add(rise)?.checked_add(*more))
                .ok_or("a total outside the exact range"),
            (Err(INEXACT), _) | (_, Err(INEXACT)) => Err(INEXACT),
            (Err(why), _) | (_, Err(why)) => Err(why),
        };
        self.float.add(bridge_float(before, after));
        self.float.absorb(&next.float);
        if fell(before, after) {
            self.resets = self.resets.saturating_add(1);
        }
        self.resets = self.resets.saturating_add(next.resets);
        let shift = elapsed_f64(self.first.0, next.first.0);
        let arriving = super::arithmetic::count(next.counted);
        self.regression.sum_x += next.regression.sum_x + arriving * shift;
        self.regression.sum_xx +=
            next.regression.sum_xx + 2.0 * shift * next.regression.sum_x + arriving * shift * shift;
        self.regression.sum_y += next.regression.sum_y;
        self.regression.sum_xy += next.regression.sum_xy + shift * next.regression.sum_y;
        self.counted = self.counted.saturating_add(next.counted);
        self.last = next.last.clone();
        self.saw_float = self.saw_float || next.saw_float;
        self.all_integer = self.all_integer && next.all_integer;
    }
}

/// The rise from `before` to `after` exactly, a fall counting from zero.
fn bridge_exact(before: &Number, after: &Number) -> Option<Decimal> {
    let (before, after) = (before.as_decimal()?, after.as_decimal()?);
    if after >= before {
        after.checked_sub(before)
    } else {
        Some(after)
    }
}

/// Whether the sample after falls below the one before — the reset rule
/// `bridge_exact` applies, read as a yes-or-no.
fn fell(before: &Number, after: &Number) -> bool {
    match (before.as_decimal(), after.as_decimal()) {
        (Some(before), Some(after)) => after < before,
        _ => approximate(after).unwrap_or(f64::NAN) < approximate(before).unwrap_or(f64::NAN),
    }
}

/// [`bridge_exact`] over the samples as floats.
fn bridge_float(before: &Number, after: &Number) -> f64 {
    let before = approximate(before).unwrap_or(f64::NAN);
    let after = approximate(after).unwrap_or(f64::NAN);
    if after >= before {
        after - before
    } else {
        after
    }
}

/// Whether a leader's summary is among the parts.
fn summarised(parts: &[Part]) -> bool {
    parts.iter().any(|part| matches!(part, Part::Summary(_)))
}

/// The parts of a group in key order, with this node's samples since the
/// last merge as the final part.
fn ordered(held: &[Sample], parts: &[Part]) -> Vec<Option<Summary>> {
    parts
        .iter()
        .map(|part| match part {
            Part::Samples(samples) => Summary::of(samples),
            Part::Summary(summary) => Some(summary.clone()),
        })
        .chain(std::iter::once(Summary::of(held)))
        .collect()
}

/// The parts joined end to end — `Some(None)` when they hold no sample — or
/// `None` when two of them interleave in time and only their samples can say
/// in which order.
fn joined(summaries: Vec<Option<Summary>>) -> Option<Option<Summary>> {
    // By first instant, equals keeping key order — the stable sort a walk of
    // the samples makes.
    let mut held: Vec<(usize, Summary)> = summaries
        .into_iter()
        .enumerate()
        .filter_map(|(place, summary)| summary.map(|summary| (place, summary)))
        .collect();
    held.sort_by_key(|(_, summary)| summary.first.0);
    let mut pieces = held.into_iter();
    let Some((mut place, mut whole)) = pieces.next() else {
        return Some(None);
    };
    for (next_place, next) in pieces {
        let follows =
            whole.last.0 < next.first.0 || (whole.last.0 == next.first.0 && place < next_place);
        if !follows {
            return None;
        }
        whole.then(&next);
        place = next_place;
    }
    Some(Some(whole))
}

/// Whether the parts merged here interleave in time, so the read must ask
/// for their samples (ADR-0121 D3).
pub(crate) fn needs_samples(fold: Aggregate, held: &[Sample], parts: &[Part]) -> bool {
    // A summary holds neither points nor age-weighted totals, so the folds
    // reading them take every part as samples; the other folds need samples
    // only when the parts interleave in time.
    summarised(parts) && (reads_samples_only(fold) || joined(ordered(held, parts)).is_none())
}

/// Whether the fold's answer is a function of the samples themselves rather
/// than of anything a summary carries.
pub(crate) fn reads_samples_only(fold: Aggregate) -> bool {
    matches!(
        fold,
        Aggregate::Downsample
            | Aggregate::DecayedSum
            | Aggregate::DecayedMean
            | Aggregate::DecayedMax
            | Aggregate::DecayedCount
    )
}

/// The fold's answer over the parts merged here and the samples held.
///
/// # Errors
///
/// [`crate::error::Error::NotSummable`] when exact arithmetic leaves its range,
/// or when parts that interleave in time reach here unasked for their samples.
pub(crate) fn finish(
    fold: Aggregate,
    held: &[Sample],
    parts: &[Part],
    ahead: Option<f64>,
    span: Span,
) -> Result<Value> {
    let name = fold.spelling();
    if matches!(
        fold,
        Aggregate::DecayedSum
            | Aggregate::DecayedMean
            | Aggregate::DecayedMax
            | Aggregate::DecayedCount
    ) {
        if summarised(parts) {
            return Err(failed(
                name,
                "parts sent as summaries rather than samples",
                span,
            ));
        }
        let mut every: Vec<Sample> = Vec::new();
        for part in parts {
            if let Part::Samples(samples) = part {
                every.extend_from_slice(samples);
            }
        }
        every.extend_from_slice(held);
        every.sort_by_key(|(at, _)| *at);
        if every.is_empty() {
            return Ok(Value::None);
        }
        let Some(half_life) = ahead.filter(|held| *held > 0.0) else {
            return Err(failed(
                name,
                "a half-life in seconds, more than nothing",
                span,
            ));
        };
        return Ok(decayed(fold, &every, half_life));
    }
    if fold == Aggregate::Downsample {
        if summarised(parts) {
            // The read asks `needs_samples` first, so a summary here is that
            // ask having been skipped — refused rather than answered from a
            // form that holds no points.
            return Err(failed(
                name,
                "parts sent as summaries rather than samples",
                span,
            ));
        }
        let mut every: Vec<Sample> = Vec::new();
        for part in parts {
            if let Part::Samples(samples) = part {
                every.extend_from_slice(samples);
            }
        }
        every.extend_from_slice(held);
        every.sort_by_key(|(at, _)| *at);
        let Some(points) = ahead
            .filter(|n| n.fract() == 0.0 && *n >= 3.0)
            .and_then(|n| rust_decimal::prelude::ToPrimitive::to_usize(&n))
        else {
            return Err(failed(
                name,
                "a whole number of points, at least three",
                span,
            ));
        };
        return Ok(Value::Array(
            largest_triangle(&every, points)
                .into_iter()
                .map(|(at, value)| {
                    let mut held = std::collections::BTreeMap::new();
                    held.insert("at".to_string(), Value::Datetime(at));
                    held.insert("value".to_string(), Value::Number(value));
                    Value::Object(held)
                })
                .collect(),
        ));
    }
    let summary = if summarised(parts) {
        joined(ordered(held, parts))
            .ok_or_else(|| failed(name, "parts that interleave in time", span))?
    } else {
        // Samples only: one run, sorted as a whole.
        let mut every: Vec<Sample> = Vec::with_capacity(held.len());
        for part in parts {
            if let Part::Samples(samples) = part {
                every.extend_from_slice(samples);
            }
        }
        every.extend_from_slice(held);
        Summary::of(&every)
    };
    let Some(summary) = summary else {
        return Ok(Value::None);
    };
    answer(fold, &summary, ahead, span)
}

/// What the fold answers for one whole run.
fn answer(fold: Aggregate, summary: &Summary, ahead: Option<f64>, span: Span) -> Result<Value> {
    // `resets` answers before the two-sample rule: a window of one sample saw
    // no falls, and zero is the complete truth there — unlike a rise, which is
    // genuinely unknown.
    if fold == Aggregate::Resets {
        return Ok(Value::Number(Number::Integer(
            i64::try_from(summary.resets).unwrap_or(i64::MAX),
        )));
    }
    if summary.counted < 2 {
        return Ok(Value::None);
    }
    if matches!(fold, Aggregate::Deriv | Aggregate::PredictLinear) {
        let counted = elapsed_count(summary.counted);
        let sums = summary.regression;
        let denominator = counted * sums.sum_xx - sums.sum_x * sums.sum_x;
        if denominator == 0.0 {
            return Ok(Value::None);
        }
        let slope = (counted * sums.sum_xy - sums.sum_x * sums.sum_y) / denominator;
        let answered = match fold {
            Aggregate::Deriv => slope,
            _ => {
                let Some(ahead) = ahead else {
                    return Err(failed(
                        fold.spelling(),
                        "a horizon, in seconds past the last sample",
                        span,
                    ));
                };
                let last = elapsed_f64(summary.first.0, summary.last.0);
                let mean_x = sums.sum_x / counted;
                let mean_y = sums.sum_y / counted;
                mean_y + slope * (last + ahead - mean_x)
            }
        };
        return Ok(Value::Number(Number::float(answered)));
    }
    let name = fold.spelling();
    let seconds = elapsed(summary.first.0, summary.last.0);
    if summary.saw_float {
        let first = approximate(&summary.first.1).unwrap_or(f64::NAN);
        let last = approximate(&summary.last.1).unwrap_or(f64::NAN);
        let rises = || summary.float.total().map_err(|why| failed(name, why, span));
        let answer = match fold {
            Aggregate::Delta => Some(last - first),
            Aggregate::Rate => {
                let per = seconds.and_then(|held| approximate(&Number::Decimal(held)));
                match per.filter(|per| *per > 0.0) {
                    Some(per) => Some(rises()? / per),
                    None => None,
                }
            }
            _ => Some(rises()?),
        };
        return Ok(answer.map_or(Value::None, |held| Value::Number(Number::float(held))));
    }
    let (Some(first), Some(last), false) = (
        summary.first.1.as_decimal(),
        summary.last.1.as_decimal(),
        summary.exact == Err(INEXACT),
    ) else {
        return Err(failed(name, INEXACT, span));
    };
    let answer = match fold {
        Aggregate::Delta => last
            .checked_sub(first)
            .ok_or_else(|| failed(name, "a difference outside the exact range", span))?,
        Aggregate::Rate => {
            let Some(per) = seconds.filter(|per| *per > Decimal::ZERO) else {
                return Ok(Value::None);
            };
            return summary
                .exact
                .ok()
                .and_then(|rise| rise.checked_div(per))
                .map_or_else(
                    || Err(failed(name, "a rate outside the exact range", span)),
                    |rate| Ok(Value::Number(Number::Decimal(rate.normalize()))),
                );
        }
        _ => summary
            .exact
            .map_err(|_| failed(name, "a total outside the exact range", span))?,
    };
    if summary.all_integer
        && let Ok(whole) = i64::try_from(answer)
    {
        return Ok(Value::Number(Number::Integer(whole)));
    }
    Ok(Value::Number(Number::Decimal(answer.normalize())))
}

/// A count as the float the regression arithmetic runs in.
fn elapsed_count(counted: u64) -> f64 {
    approximate(&Number::Integer(i64::try_from(counted).unwrap_or(i64::MAX))).unwrap_or(f64::NAN)
}

/// The seconds between two instants, as the float the regression runs in.
fn elapsed_f64(first: Datetime, last: Datetime) -> f64 {
    elapsed(first, last).map_or(f64::NAN, |held| {
        approximate(&Number::Decimal(held)).unwrap_or(f64::NAN)
    })
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

/// The largest-triangle down-sampling: the first and last samples always,
/// and between them the point of each of `points - 2` equal buckets that
/// makes the largest triangle with the previously chosen point and the next
/// bucket's average — the shape's sharpest survivors. `x` runs in seconds
/// from the first sample, as the regression's does.
fn largest_triangle(ordered: &[Sample], points: usize) -> Vec<Sample> {
    if ordered.len() <= points {
        return ordered.to_vec();
    }
    let last = ordered.len().saturating_sub(1);
    let x = |at: Datetime| elapsed_f64(ordered[0].0, at);
    let y = |value: &Number| approximate(value).unwrap_or(f64::NAN);
    // Both are non-zero here: `points` is at least three and the series is
    // longer than it, so the divisions below never see a zero divisor.
    let interior = ordered.len().saturating_sub(2);
    let buckets = points.saturating_sub(2);
    let share = |upto: usize| {
        upto.saturating_mul(interior)
            .checked_div(buckets)
            .unwrap_or(0)
            .saturating_add(1)
    };
    let mut chosen: Vec<Sample> = vec![ordered[0].clone()];
    for bucket in 0..buckets {
        let from = share(bucket);
        let to = share(bucket.saturating_add(1));
        let (next_from, next_to) = (to, share(bucket.saturating_add(2)).min(last).max(to));
        let average = if next_from < next_to {
            let span = wide(next_to.saturating_sub(next_from));
            let x_mean = (next_from..next_to).map(|i| x(ordered[i].0)).sum::<f64>() / span;
            let y_mean = (next_from..next_to).map(|i| y(&ordered[i].1)).sum::<f64>() / span;
            (x_mean, y_mean)
        } else {
            (x(ordered[last].0), y(&ordered[last].1))
        };
        let (ax, ay) = (
            x(chosen[chosen.len().saturating_sub(1)].0),
            y(&chosen[chosen.len().saturating_sub(1)].1),
        );
        let mut best = from;
        let mut best_area = -1.0_f64;
        let upto = to.max(from.saturating_add(1)).min(ordered.len());
        for (candidate, sample) in ordered.iter().enumerate().take(upto).skip(from) {
            let (bx, by) = (x(sample.0), y(&sample.1));
            let area = ((ax - average.0) * (by - ay) - (ax - bx) * (average.1 - ay)).abs();
            if area > best_area {
                best_area = area;
                best = candidate;
            }
        }
        chosen.push(ordered[best].clone());
    }
    chosen.push(ordered[last].clone());
    chosen
}

/// A place count as the float the geometry runs in.
fn wide(held: usize) -> f64 {
    super::arithmetic::count(u64::try_from(held).unwrap_or(u64::MAX))
}

/// The age-weighted answers: each sample weighs `0.5` to the power of its
/// age in half-lives, measured back from the newest sample. Floats by
/// declaration — a weight is a power, and no exact kind holds one.
fn decayed(fold: Aggregate, ordered: &[Sample], half_life: f64) -> Value {
    let Some((newest, _)) = ordered.last() else {
        return Value::None;
    };
    let mut weights = Vec::with_capacity(ordered.len());
    let mut values = Vec::with_capacity(ordered.len());
    for (at, value) in ordered {
        let age = elapsed_f64(*at, *newest);
        weights.push(0.5_f64.powf(age / half_life));
        values.push(approximate(value).unwrap_or(f64::NAN));
    }
    let weight_total: f64 = weights.iter().sum();
    let answered = match fold {
        Aggregate::DecayedSum => weights.iter().zip(&values).map(|(w, v)| w * v).sum(),
        Aggregate::DecayedMean => {
            if weight_total == 0.0 {
                return Value::None;
            }
            weights.iter().zip(&values).map(|(w, v)| w * v).sum::<f64>() / weight_total
        }
        Aggregate::DecayedMax => weights
            .iter()
            .zip(&values)
            .map(|(w, v)| w * v)
            .fold(f64::NEG_INFINITY, f64::max),
        _ => weight_total,
    };
    Value::Number(Number::float(answered))
}
