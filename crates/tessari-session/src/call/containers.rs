//! The functions that read what a space key holds: an element or a field
//! chosen by a value, sets of members, and rankings (ADR-0135).
//!
//! A container is one value, so each of these is a computation over the value
//! `GET k` already answered — the read costs what the `GET` cost, and the
//! function only decides what of it comes back.
//!
//! # Two shapes of a ranked member, on purpose
//!
//! A ranking is held as a set of `[score, member]` pairs, which is what makes
//! the set's own order the ranking. A function that answers a RANKING —
//! `scores::union`, `scores::intersect` — answers that stored form, so
//! `SET k = scores::union(…)` leaves a key a `SCORES` statement reads. A
//! function that answers MEMBERS answers `[member, score]` pairs, the shape
//! `SCORES POP` already answers.

use std::collections::{BTreeSet, HashMap};

use tessari_ql::{Function, Span};
use tessari_types::{Number, Value};

use super::arguments::{named, whole, wrong_type};
use super::{array_at, count, object_at, text_at};
use crate::error::{Error, Result};
use crate::kv::{is_ranking, span_of};

/// `array::at(items, i)` — a negative `i` counted from the back.
pub(super) fn element(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let items = array_at(function, arguments, 0, span)?;
    let at = whole(function, arguments, 1, span)?;
    let position = if at < 0 {
        i64::try_from(items.len())
            .ok()
            .and_then(|length| length.checked_add(at))
    } else {
        Some(at)
    };
    Ok(position
        .and_then(|position| usize::try_from(position).ok())
        .and_then(|position| items.get(position))
        .cloned()
        .unwrap_or(Value::None))
}

/// `object::get(o, name)`.
pub(super) fn field(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let fields = object_at(function, arguments, 0, span)?;
    let name = text_at(function, arguments, 1, span)?;
    Ok(fields.get(name).cloned().unwrap_or(Value::None))
}

/// `set::len(s)` — any set, a ranking counting its members.
pub(super) fn set_len(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    match arguments.first() {
        Some(Value::Set(elements)) => count(elements.len(), function, span),
        other => Err(wrong_type(function, 0, "a set", named(other), span)),
    }
}

/// `set::union`, `set::intersect` and `set::difference`.
pub(super) fn combined(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let left = members_at(function, arguments, 0, span)?;
    let right = members_at(function, arguments, 1, span)?;
    let answer: BTreeSet<Value> = match function {
        Function::SetUnion => left.union(right).cloned().collect(),
        Function::SetIntersect => left.intersection(right).cloned().collect(),
        _ => left.difference(right).cloned().collect(),
    };
    Ok(Value::Set(answer))
}

/// `scores::score(r, member)`.
pub(super) fn score(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let ranked = ranking_at(function, arguments, 0, span)?;
    let member = text_at(function, arguments, 1, span)?;
    Ok(ranked
        .iter()
        .find(|(_, name)| *name == member)
        .map_or(Value::None, |(held, _)| float(*held)))
}

/// `scores::rank(r, member)` — from the lowest score, from 0.
pub(super) fn rank(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let ranked = ranking_at(function, arguments, 0, span)?;
    let member = text_at(function, arguments, 1, span)?;
    match ranked.iter().position(|(_, name)| *name == member) {
        Some(at) => count(at, function, span),
        None => Ok(Value::None),
    }
}

/// `scores::range(r, start, stop)` — places counted as `SCORES REMOVE RANK`
/// counts them.
pub(super) fn range(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let ranked = ranking_at(function, arguments, 0, span)?;
    let start = whole(function, arguments, 1, span)?;
    let stop = whole(function, arguments, 2, span)?;
    let Some((from, to)) = span_of(ranked.len(), start, stop) else {
        return Ok(Value::Array(Vec::new()));
    };
    Ok(answered(
        ranked
            .iter()
            .skip(from)
            .take(to.saturating_sub(from).saturating_add(1)),
    ))
}

/// `scores::between(r, low, high)` — an absent bound is open, an absent ranking
/// answers `NONE` (the general rule is not applied to this function).
pub(super) fn between(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    if arguments
        .first()
        .is_none_or(|held| !held.is_present() || *held == Value::Null)
    {
        return Ok(Value::None);
    }
    let ranked = ranking_at(function, arguments, 0, span)?;
    let low = bound(function, arguments, 1, span)?;
    let high = bound(function, arguments, 2, span)?;
    Ok(answered(ranked.iter().filter(|(held, _)| {
        low.is_none_or(|low| *held >= low) && high.is_none_or(|high| *held <= high)
    })))
}

/// `scores::union` and `scores::intersect` — scores added, answered as a ranking.
pub(super) fn ranked(function: Function, arguments: &[Value], span: Span) -> Result<Value> {
    let left = ranking_at(function, arguments, 0, span)?;
    let right = ranking_at(function, arguments, 1, span)?;
    let right: HashMap<&str, f64> = right.into_iter().map(|(held, name)| (name, held)).collect();
    let mut sums: HashMap<&str, f64> = HashMap::new();
    for (held, name) in &left {
        match right.get(name) {
            Some(other) => {
                sums.insert(name, held + other);
            }
            None if function == Function::ScoresUnion => {
                sums.insert(name, *held);
            }
            None => {}
        }
    }
    if function == Function::ScoresUnion {
        for (name, held) in &right {
            sums.entry(name).or_insert(*held);
        }
    }
    let mut answer = BTreeSet::new();
    for (name, sum) in sums {
        if !sum.is_finite() {
            return Err(Error::CallFailed {
                function,
                reason: "an added score is beyond what a float holds",
                span,
            });
        }
        answer.insert(Value::Array(vec![float(sum), Value::from(name)]));
    }
    Ok(Value::Set(answer))
}

/// A set of members, refusing a ranking by name — its elements are pairs, and
/// combining pairs is not combining rankings.
fn members_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<&BTreeSet<Value>> {
    match arguments.get(at) {
        Some(Value::Set(members)) if is_ranking(members) => Err(wrong_type(
            function,
            at,
            "a set of members",
            "a ranking",
            span,
        )),
        Some(Value::Set(members)) => Ok(members),
        other => Err(wrong_type(
            function,
            at,
            "a set of members",
            named(other),
            span,
        )),
    }
}

/// A ranking's members, lowest score first, as `(score, member)`.
fn ranking_at(
    function: Function,
    arguments: &[Value],
    at: usize,
    span: Span,
) -> Result<Vec<(f64, &str)>> {
    let refused = |found| wrong_type(function, at, "a ranking", found, span);
    match arguments.get(at) {
        Some(Value::Set(pairs)) if is_ranking(pairs) => Ok(pairs
            .iter()
            .filter_map(|pair| match pair {
                Value::Array(pair) => match pair.as_slice() {
                    [Value::Number(Number::Float(held)), Value::String(name)] => {
                        Some((*held, name.as_str()))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()),
        Some(Value::Set(_)) => Err(refused("a set of members")),
        other => Err(refused(named(other))),
    }
}

/// A score bound, `None` when absent — an open side.
fn bound(function: Function, arguments: &[Value], at: usize, span: Span) -> Result<Option<f64>> {
    match arguments.get(at) {
        None | Some(Value::None | Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_float().map(Some).ok_or(Error::CallFailed {
            function,
            reason: "a bound is a number a float holds",
            span,
        }),
        other => Err(wrong_type(function, at, "a number", named(other), span)),
    }
}

/// Ranked members as a read answers them, `[member, score]`.
fn answered<'a>(members: impl Iterator<Item = &'a (f64, &'a str)>) -> Value {
    Value::Array(
        members
            .map(|(held, name)| Value::Array(vec![Value::from(*name), float(*held)]))
            .collect(),
    )
}

fn float(held: f64) -> Value {
    Value::Number(Number::float(held))
}
