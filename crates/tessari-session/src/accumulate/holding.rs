//! What the holding folds send from a shard's leader, and how they merge here
//! in key order into the answer one walk gives (ADR-0121).
//!
//! - `median` sends its values as sorted runs `[[exact value, how many], …]`;
//!   the answer depends on the multiset of exact values alone (D1).
//! - `collect` sends its values in key order; the parts concatenated in key
//!   order are the walk (D2).
//! - A counter fold sends `["summary", …]` — how many samples, the first and
//!   the last, its rises exactly and as floats — or, asked again because two
//!   shards interleave in time, `["samples", [[value, instant], …]]` (D3).

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use tessari_types::{Number, Value};

use super::counter::{INEXACT, Part, Sample, Summary};
use super::exact::ExactSum;
use super::{Accumulator, exact};

/// The tag a counter summary travels under.
const SUMMARY: &str = "summary";

/// The tag a counter's samples travel under.
const SAMPLES: &str = "samples";

impl Accumulator {
    /// A holding fold's state, or `None` for any other fold or when it
    /// cannot travel exactly (a value no decimal holds, a counter whose exact
    /// form failed with no float to answer instead).
    pub(super) fn holding_state(&self) -> Option<Value> {
        match self {
            Self::Middle { held, runs, span } => {
                let mut counted = runs.clone();
                for value in held {
                    let entry = counted.entry(exact(value, *span).ok()?).or_insert(0);
                    *entry = entry.saturating_add(1);
                }
                let mut sent = Vec::with_capacity(counted.len());
                for (value, many) in counted {
                    sent.push(Value::Array(vec![
                        Value::Number(Number::Decimal(value.normalize())),
                        Value::Number(Number::Integer(i64::try_from(many).ok()?)),
                    ]));
                }
                Some(Value::Array(sent))
            }
            Self::Every { held } => Some(Value::Array(held.clone())),
            Self::Counter { held, parts, .. } if parts.is_empty() => {
                let Some(summary) = Summary::of(held) else {
                    return Some(Value::Array(vec![Value::from(SUMMARY)]));
                };
                summary_state(&summary)
            }
            _ => None,
        }
    }

    /// A counter fold's samples as they travel when its summary cannot join
    /// the others (ADR-0121 D3); `None` for any other fold.
    pub(crate) fn samples(&self) -> Option<Value> {
        let Self::Counter { held, parts, .. } = self else {
            return None;
        };
        if !parts.is_empty() {
            return None;
        }
        let sent = held
            .iter()
            .map(|(at, value)| {
                Value::Array(vec![Value::Number(value.clone()), Value::Datetime(*at)])
            })
            .collect();
        Some(Value::Array(vec![Value::from(SAMPLES), Value::Array(sent)]))
    }

    /// Whether the parts merged into a counter fold interleave in time, so
    /// the read must ask the leaders for their samples.
    pub(crate) fn needs_samples(&self) -> bool {
        match self {
            Self::Counter { held, parts, .. } => super::counter::needs_samples(held, parts),
            _ => false,
        }
    }

    /// How many entries a holding fold keeps beyond its group — one per
    /// `median` run or value, per `collect` value, per counter sample; a
    /// leader's counter summary keeps none (ADR-0121 D5). What every other fold
    /// keeps is constant and is its group's.
    pub(crate) fn retained(&self) -> usize {
        match self {
            Self::Count { .. }
            | Self::Sum { .. }
            | Self::Mean { .. }
            | Self::Spread { .. }
            | Self::Extreme { .. } => 0,
            Self::Middle { held, runs, .. } => held.len().saturating_add(runs.len()),
            Self::Every { held } => held.len(),
            Self::Counter { held, parts, .. } => parts
                .iter()
                .map(|part| match part {
                    Part::Samples(samples) => samples.len(),
                    Part::Summary(_) => 0,
                })
                .fold(held.len(), usize::saturating_add),
        }
    }

    /// Merge a holding fold's state; `None` when this is not a holding fold,
    /// `Some(false)` for a state of another shape.
    pub(super) fn merge_holding(&mut self, state: &Value) -> Option<bool> {
        match self {
            Self::Middle { runs, .. } => Some(runs_of(state).is_some_and(|more| {
                for (value, many) in more {
                    let entry = runs.entry(value).or_insert(0);
                    *entry = entry.saturating_add(many);
                }
                true
            })),
            Self::Every { held } => Some(match state {
                Value::Array(more) => {
                    held.extend_from_slice(more);
                    true
                }
                _ => false,
            }),
            Self::Counter { held, parts, .. } => Some(match part_of(state) {
                Some(part) => {
                    // This node's samples so far precede the part in key order.
                    if !held.is_empty() {
                        parts.push(Part::Samples(std::mem::take(held)));
                    }
                    if let Some(part) = part {
                        parts.push(part);
                    }
                    true
                }
                None => false,
            }),
            _ => None,
        }
    }
}

/// A summary as it travels: `["summary", how many, [first], [last], exact,
/// float, saw a float, all integers]`; `None` when its exact form failed and
/// no float decides the answer, which the walk must then refuse in its own
/// words.
fn summary_state(summary: &Summary) -> Option<Value> {
    let exact = match &summary.exact {
        Ok(total) => Value::Number(Number::Decimal(*total)),
        Err(_) if summary.saw_float => Value::None,
        Err(_) => return None,
    };
    let sample = |(at, value): &Sample| {
        Value::Array(vec![Value::Number(value.clone()), Value::Datetime(*at)])
    };
    Some(Value::Array(vec![
        Value::from(SUMMARY),
        Value::Number(Number::Integer(i64::try_from(summary.counted).ok()?)),
        sample(&summary.first),
        sample(&summary.last),
        exact,
        summary.float.state()?,
        Value::Bool(summary.saw_float),
        Value::Bool(summary.all_integer),
    ]))
}

/// A median's runs.
fn runs_of(state: &Value) -> Option<BTreeMap<Decimal, u64>> {
    let Value::Array(sent) = state else {
        return None;
    };
    let mut runs = BTreeMap::new();
    for run in sent {
        let Value::Array(run) = run else {
            return None;
        };
        let [
            Value::Number(Number::Decimal(value)),
            Value::Number(Number::Integer(many)),
        ] = run.as_slice()
        else {
            return None;
        };
        let entry = runs.entry(*value).or_insert(0_u64);
        *entry = entry.saturating_add(u64::try_from(*many).ok()?);
    }
    Some(runs)
}

/// One `[value, instant]` sample.
fn sample_of(sent: &Value) -> Option<Sample> {
    let Value::Array(pair) = sent else {
        return None;
    };
    let [Value::Number(value), Value::Datetime(at)] = pair.as_slice() else {
        return None;
    };
    Some((*at, value.clone()))
}

/// A counter part — `Some(None)` for a shard that held no sample of the
/// group, `None` for a state of another shape.
fn part_of(state: &Value) -> Option<Option<Part>> {
    let Value::Array(sent) = state else {
        return None;
    };
    match sent.as_slice() {
        [Value::String(tag)] if tag == SUMMARY => Some(None),
        [Value::String(tag), Value::Array(samples)] if tag == SAMPLES => {
            let samples: Option<Vec<Sample>> = samples.iter().map(sample_of).collect();
            Some(Some(Part::Samples(samples?)))
        }
        [
            Value::String(tag),
            Value::Number(Number::Integer(counted)),
            first,
            last,
            exact,
            float,
            Value::Bool(saw_float),
            Value::Bool(all_integer),
        ] if tag == SUMMARY => {
            let exact = match exact {
                Value::Number(Number::Decimal(total)) => Ok(*total),
                // Only a part that met a float may send no exact form.
                Value::None if *saw_float => Err(INEXACT),
                _ => return None,
            };
            Some(Some(Part::Summary(Summary {
                counted: u64::try_from(*counted)
                    .ok()
                    .filter(|counted| *counted > 0)?,
                first: sample_of(first)?,
                last: sample_of(last)?,
                exact,
                float: ExactSum::from_state(float)?,
                saw_float: *saw_float,
                all_integer: *all_integer,
            })))
        }
        _ => None,
    }
}
