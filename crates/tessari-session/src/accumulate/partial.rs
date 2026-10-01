//! What a fold holds, as a value that can travel to another node and be folded
//! in there (ADR-0097 D2).
//!
//! Only the folds whose merge is the same as offering the values one after
//! another have a state: `count`, `sum` and `mean` over exact numbers, `min` and
//! `max`. Merging in key order is then indistinguishable from one walk — the
//! counts and the exact totals add, and an extreme offered as a value keeps the
//! first of two equals for `min` and the last for `max`, as a walk does.

use rust_decimal::Decimal;
use tessari_types::{Number, Value};

use super::{Accumulator, add_exact, add_float};
use crate::error::Result;

impl Accumulator {
    /// What this fold holds, or `None` when it cannot travel exactly.
    ///
    /// `count` is the count; `sum` is `[total, all integers]`; `mean` is
    /// `[total, how many]`; `min` and `max` the value held, or `NONE`.
    pub(crate) fn state(&self) -> Option<Value> {
        match self {
            Self::Count { seen } => i64::try_from(*seen)
                .ok()
                .map(|seen| Value::Number(Number::Integer(seen))),
            Self::Sum {
                exact: Ok(total),
                saw_float: false,
                all_integer,
                ..
            } => Some(Value::Array(vec![
                Value::Number(Number::Decimal(*total)),
                Value::Bool(*all_integer),
            ])),
            Self::Mean {
                exact: Ok(total),
                counted,
                ..
            } => Some(Value::Array(vec![
                Value::Number(Number::Decimal(*total)),
                Value::Number(Number::Integer(*counted)),
            ])),
            Self::Extreme { held, .. } => Some(held.clone().unwrap_or(Value::None)),
            _ => None,
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
                all_integer,
                ..
            } => {
                let Some((total, integer)) =
                    pair(state).and_then(|(total, integer)| match integer {
                        Value::Bool(integer) => Some((total, *integer)),
                        _ => None,
                    })
                else {
                    return Ok(false);
                };
                let total = Number::Decimal(total);
                add_exact(exact, &total);
                add_float(float, &total);
                *all_integer = *all_integer && integer;
            }
            Self::Mean { exact, counted, .. } => {
                let Some((total, many)) =
                    pair(state).and_then(|(total, many)| Some((total, whole(many)?)))
                else {
                    return Ok(false);
                };
                add_exact(exact, &Number::Decimal(total));
                *counted = counted.saturating_add(many);
            }
            Self::Extreme { .. } => self.offer(state)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// An integer state.
fn whole(state: &Value) -> Option<i64> {
    match state {
        Value::Number(Number::Integer(held)) => Some(*held),
        _ => None,
    }
}

/// A `[total, second]` state.
fn pair(state: &Value) -> Option<(Decimal, &Value)> {
    match state {
        Value::Array(held) => match held.as_slice() {
            [Value::Number(Number::Decimal(total)), second] => Some((*total, second)),
            _ => None,
        },
        _ => None,
    }
}
