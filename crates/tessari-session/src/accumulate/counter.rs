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
pub(crate) fn needs_samples(held: &[Sample], parts: &[Part]) -> bool {
    summarised(parts) && joined(ordered(held, parts)).is_none()
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
    span: Span,
) -> Result<Value> {
    let name = fold.spelling();
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
    answer(fold, &summary, span)
}

/// What the fold answers for one whole run.
fn answer(fold: Aggregate, summary: &Summary, span: Span) -> Result<Value> {
    if summary.counted < 2 {
        return Ok(Value::None);
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

/// The seconds between two instants, exactly.
fn elapsed(first: Datetime, last: Datetime) -> Option<Decimal> {
    let whole = last.seconds().checked_sub(first.seconds())?;
    let nanos = i64::from(last.nanos()).checked_sub(i64::from(first.nanos()))?;
    let total = i128::from(whole)
        .checked_mul(i128::from(NANOS))?
        .checked_add(i128::from(nanos))?;
    Decimal::from_i128_with_scale(total, 9).into()
}
