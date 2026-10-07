//! What a fold holds, as a value that can travel to another node and be folded
//! in there (ADR-0097 D2, ADR-0114 D5).
//!
//! Only the folds whose merge is the same as offering the values one after
//! another have a state: `count`; `sum` and `mean`, whose exact total and whose
//! float total both add in any order; `variance` and `stddev`, whose count and
//! two exact float totals do too; `min` and `max`. Merging in key order is then
//! indistinguishable from one walk — the counts and the totals add, and an
//! extreme offered as a value keeps the first of two equals for `min` and the
//! last for `max`, as a walk does.
//!
//! A total carries both of its forms because which one answers is decided by
//! the whole group: a shard that saw only integers still owes its float total
//! to a group in which another shard met a float.
//!
//! A leader still running `0.24` sends a total as `[exact, second]`, with no
//! float form — it declined any shard that met a float, so its part is exact.
//! That part merges; if the group then turns out to need a float answer, the
//! float total it never sent is [`UNSENT`], and the read gathers records
//! instead ([`Accumulator::lacks_float`]).

use rust_decimal::Decimal;
use tessari_types::{Number, Value};

use super::{Accumulator, ExactSum, Moments, Running, add_exact};
use crate::error::Result;

/// Why a merged float total cannot answer: a part arrived without one.
const UNSENT: &str = "a part whose float total was not sent";

impl Accumulator {
    /// What this fold holds, or `None` when it cannot travel exactly.
    ///
    /// `count` is the count; `sum` is `[exact, all integers, saw a float,
    /// float]`; `mean` is `[exact, how many, saw a float, float]`; `variance`
    /// and `stddev` are `[how many, total, squares]`; `min` and `max` the value
    /// held, or `NONE`. An exact total that failed travels as `NONE` when a
    /// float already decides the answer, and otherwise declines.
    pub(crate) fn state(&self) -> Option<Value> {
        match self {
            Self::Count { seen } => i64::try_from(*seen)
                .ok()
                .map(|seen| Value::Number(Number::Integer(seen))),
            Self::Sum {
                exact,
                float: Ok(float),
                saw_float,
                all_integer,
                ..
            } => Some(Value::Array(vec![
                exact_state(exact, *saw_float)?,
                Value::Bool(*all_integer),
                Value::Bool(*saw_float),
                float.state()?,
            ])),
            Self::Mean {
                exact,
                float: Ok(float),
                saw_float,
                counted,
                ..
            } => Some(Value::Array(vec![
                exact_state(exact, *saw_float)?,
                Value::Number(Number::Integer(*counted)),
                Value::Bool(*saw_float),
                float.state()?,
            ])),
            Self::Spread {
                running: Ok(moments),
                ..
            } => Some(Value::Array(vec![
                Value::Number(Number::Integer(i64::try_from(moments.counted).ok()?)),
                moments.total.state()?,
                moments.squares.state()?,
            ])),
            Self::Extreme { held, .. } => Some(held.clone().unwrap_or(Value::None)),
            Self::Distinct { .. } | Self::Quantile { .. } => self.sketch_state(),
            _ => self.holding_state(),
        }
    }

    /// Whether this total must answer as a float and lacks a part's float total
    /// — the one merge whose answer only the records can give.
    pub(crate) fn lacks_float(&self) -> bool {
        match self {
            Self::Sum {
                float, saw_float, ..
            }
            | Self::Mean {
                float, saw_float, ..
            } => *saw_float && float.as_ref().err() == Some(&UNSENT),
            _ => false,
        }
    }

    /// Fold in a state another accumulator of the same fold reached, as if its
    /// values had been offered here next; `false` for a state of another shape.
    ///
    /// # Errors
    ///
    /// Whatever offering the held value of an extreme would raise.
    pub(crate) fn merge(&mut self, state: &Value) -> Result<bool> {
        match self {
            Self::Count { seen } => {
                let Some(more) = whole(state).and_then(|more| u64::try_from(more).ok()) else {
                    return Ok(false);
                };
                *seen = seen.saturating_add(more);
            }
            Self::Sum {
                exact,
                float,
                saw_float,
                all_integer,
                ..
            } => {
                let Some((total, integer, floated, more)) = totals(state, |second| match second {
                    Value::Bool(integer) => Some(*integer),
                    _ => None,
                }) else {
                    return Ok(false);
                };
                merge_exact(exact, total);
                merge_float(float, more.as_ref());
                *all_integer = *all_integer && integer;
                *saw_float = *saw_float || floated;
            }
            Self::Mean {
                exact,
                float,
                saw_float,
                counted,
                ..
            } => {
                let Some((total, many, floated, more)) = totals(state, whole) else {
                    return Ok(false);
                };
                merge_exact(exact, total);
                merge_float(float, more.as_ref());
                *counted = counted.saturating_add(many);
                *saw_float = *saw_float || floated;
            }
            Self::Spread { running, .. } => {
                let Some(more) = moments(state) else {
                    return Ok(false);
                };
                if let Ok(held) = running {
                    held.absorb(&more);
                }
            }
            Self::Extreme { .. } => self.offer(state)?,
            Self::Distinct { .. } | Self::Quantile { .. } => return Ok(self.merge_sketch(state)),
            _ => return Ok(self.merge_holding(state).unwrap_or(false)),
        }
        Ok(true)
    }
}

/// An exact total as it travels: the decimal, or `NONE` when it failed and a
/// float decides the answer anyway; `None` (decline) when it failed and would
/// have been the answer.
fn exact_state(exact: &Running<Decimal>, saw_float: bool) -> Option<Value> {
    match exact {
        Ok(total) => Some(Value::Number(Number::Decimal(*total))),
        Err(_) if saw_float => Some(Value::None),
        Err(_) => None,
    }
}

/// A `[exact, second, saw a float, float]` state, its second read by `second`;
/// or a `0.24` leader's `[exact, second]`, which met no float and sent no float
/// total.
fn totals<T>(
    state: &Value,
    second: impl FnOnce(&Value) -> Option<T>,
) -> Option<(Option<Decimal>, T, bool, Option<ExactSum>)> {
    let Value::Array(held) = state else {
        return None;
    };
    if let [Value::Number(Number::Decimal(total)), other] = held.as_slice() {
        return Some((Some(*total), second(other)?, false, None));
    }
    let [exact, other, Value::Bool(floated), float] = held.as_slice() else {
        return None;
    };
    let exact = match exact {
        Value::Number(Number::Decimal(total)) => Some(*total),
        // Only a part that met a float may send no exact total.
        Value::None if *floated => None,
        _ => return None,
    };
    Some((
        exact,
        second(other)?,
        *floated,
        Some(ExactSum::from_state(float)?),
    ))
}

/// Add another part's exact total; a part with none makes this one failed,
/// which only a float answer — and that part had a float — can then follow.
fn merge_exact(exact: &mut Running<Decimal>, total: Option<Decimal>) {
    match total {
        Some(total) => add_exact(exact, &Number::Decimal(total)),
        None => *exact = Err("a total outside the exact range"),
    }
}

/// Add another part's float total; a part that sent none leaves this one
/// [`UNSENT`].
fn merge_float(float: &mut Running<ExactSum>, more: Option<&ExactSum>) {
    match (float.as_mut(), more) {
        (Ok(held), Some(more)) => held.absorb(more),
        (_, None) => *float = Err(UNSENT),
        (Err(_), Some(_)) => {}
    }
}

/// A `[how many, total, squares]` state.
fn moments(state: &Value) -> Option<Moments> {
    let Value::Array(held) = state else {
        return None;
    };
    let [counted, total, squares] = held.as_slice() else {
        return None;
    };
    Some(Moments {
        counted: u64::try_from(whole(counted)?).ok()?,
        total: ExactSum::from_state(total)?,
        squares: ExactSum::from_state(squares)?,
    })
}

/// An integer state.
fn whole(state: &Value) -> Option<i64> {
    match state {
        Value::Number(Number::Integer(held)) => Some(*held),
        _ => None,
    }
}
