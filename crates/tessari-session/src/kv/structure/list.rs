//! `LIST PUSH | POP | SET | TRIM | REMOVE | INSERT | MOVE`.
//!
//! Indexes count from the front at zero and from the back at `-1`, the way a
//! cache client counts them, so `LIST TRIM k FROM 0 TO -1` keeps everything.

use tessari_ql::{Container, ListEnd, RecordTarget, Span, StructureOp};
use tessari_storage::Transaction;
use tessari_types::{Number, Value};

use super::counted;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// One `LIST` statement.
    pub(super) async fn list(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        op: &StructureOp,
        span: Span,
    ) -> Result<Outcome> {
        let held = self.held(transaction, target, Container::List).await?;
        let mut items = elements(&held.value);
        let answer = match op {
            StructureOp::ListPush {
                end,
                values,
                if_present,
            } => {
                if *if_present && held.value == Value::None {
                    return Ok(Outcome::Value(counted(0)));
                }
                let values = self.evaluated(transaction, values).await?;
                match end {
                    ListEnd::Back => items.extend(values),
                    ListEnd::Front => values.into_iter().for_each(|value| items.insert(0, value)),
                }
                counted(items.len())
            }
            StructureOp::ListPop { end, limit } => {
                let limit = self.limit(transaction, limit.as_ref()).await?;
                let taken = take(&mut items, *end, limit.unwrap_or(1));
                if taken.is_empty() {
                    return Ok(Outcome::Value(match limit {
                        Some(_) => Value::Array(Vec::new()),
                        None => Value::None,
                    }));
                }
                match limit {
                    Some(_) => Value::Array(taken),
                    None => taken.into_iter().next().unwrap_or(Value::None),
                }
            }
            StructureOp::ListSet { index, value } => {
                let key = held.address.id.to_string();
                if held.value == Value::None {
                    return Err(Error::NoSuchList {
                        key,
                        span: target.span,
                    });
                }
                let at = self.whole_number(transaction, index, "index").await?;
                let Some(slot) = position(at, items.len()) else {
                    return Err(Error::ListIndexOutOfRange {
                        key,
                        index: at,
                        length: u64::try_from(items.len()).unwrap_or(u64::MAX),
                        span: index.span,
                    });
                };
                items[slot] = self.evaluate(transaction, value).await?;
                Value::None
            }
            StructureOp::ListTrim { start, stop } => {
                let start = self.whole_number(transaction, start, "index").await?;
                let stop = self.whole_number(transaction, stop, "index").await?;
                items = kept(items, start, stop);
                Value::None
            }
            StructureOp::ListRemove {
                value,
                limit,
                from_back,
            } => {
                let value = self.evaluate(transaction, value).await?;
                let limit = self.limit(transaction, limit.as_ref()).await?;
                counted(remove(&mut items, &value, limit, *from_back))
            }
            StructureOp::ListInsert {
                value,
                after,
                pivot,
            } => {
                if held.value == Value::None {
                    return Ok(Outcome::Value(counted(0)));
                }
                let value = self.evaluate(transaction, value).await?;
                let pivot = self.evaluate(transaction, pivot).await?;
                let Some(at) = items.iter().position(|item| *item == pivot) else {
                    return Ok(Outcome::Value(Value::Number(Number::Integer(-1))));
                };
                items.insert(if *after { at.saturating_add(1) } else { at }, value);
                counted(items.len())
            }
            StructureOp::ListMove { from, to, to_end } => {
                return self
                    .list_move(transaction, (held, items), (*from, to, *to_end), span)
                    .await;
            }
            _ => return Ok(Outcome::Done),
        };
        let unchanged = held.value == Value::None && items.is_empty();
        if !unchanged {
            self.keep(
                transaction,
                held,
                (!items.is_empty()).then_some(Value::Array(items)),
                span,
            )
            .await?;
        }
        Ok(match op {
            StructureOp::ListSet { .. } | StructureOp::ListTrim { .. } => Outcome::Done,
            _ => Outcome::Value(answer),
        })
    }

    /// `LIST MOVE`: take from one end of this list and put at an end of
    /// another — or of this one, which turns it.
    async fn list_move(
        &self,
        transaction: &mut Transaction<'_>,
        (held, mut items): (super::Held, Vec<Value>),
        (from, to, to_end): (ListEnd, &RecordTarget, ListEnd),
        span: Span,
    ) -> Result<Outcome> {
        let Some(moved) = take(&mut items, from, 1).pop() else {
            return Ok(Outcome::Value(Value::None));
        };
        let (_, destination) = self.writable(transaction, to).await?;
        if destination == held.address {
            put(&mut items, to_end, moved.clone());
            self.keep(transaction, held, Some(Value::Array(items)), span)
                .await?;
            return Ok(Outcome::Value(moved));
        }
        let other = self.held(transaction, to, Container::List).await?;
        let mut theirs = elements(&other.value);
        put(&mut theirs, to_end, moved.clone());
        self.keep(
            transaction,
            held,
            (!items.is_empty()).then_some(Value::Array(items)),
            span,
        )
        .await?;
        self.keep(transaction, other, Some(Value::Array(theirs)), span)
            .await?;
        Ok(Outcome::Value(moved))
    }
}

/// The elements a held list has, or none for no key.
fn elements(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

/// Up to `count` elements off `end`, in the order they come off.
fn take(items: &mut Vec<Value>, end: ListEnd, count: usize) -> Vec<Value> {
    let count = count.min(items.len());
    match end {
        ListEnd::Front => items.drain(..count).collect(),
        ListEnd::Back => {
            let rest = items.len().saturating_sub(count);
            let mut taken: Vec<Value> = items.drain(rest..).collect();
            taken.reverse();
            taken
        }
    }
}

fn put(items: &mut Vec<Value>, end: ListEnd, value: Value) {
    match end {
        ListEnd::Front => items.insert(0, value),
        ListEnd::Back => items.push(value),
    }
}

/// The slot an index names in a list of `length`, counting a negative one from
/// the back.
fn position(index: i64, length: usize) -> Option<usize> {
    let length = i64::try_from(length).ok()?;
    let at = if index < 0 {
        length.checked_add(index)?
    } else {
        index
    };
    if (0..length).contains(&at) {
        usize::try_from(at).ok()
    } else {
        None
    }
}

/// The elements from `start` to `stop`, both kept, each clamped to the list.
fn kept(items: Vec<Value>, start: i64, stop: i64) -> Vec<Value> {
    let Ok(length) = i64::try_from(items.len()) else {
        return items;
    };
    let from_back = |index: i64| {
        if index < 0 {
            length.saturating_add(index)
        } else {
            index
        }
    };
    let start = from_back(start).max(0);
    let stop = from_back(stop).min(length.saturating_sub(1));
    if start > stop {
        return Vec::new();
    }
    let (Ok(start), Ok(stop)) = (usize::try_from(start), usize::try_from(stop)) else {
        return Vec::new();
    };
    items
        .into_iter()
        .skip(start)
        .take(stop.saturating_sub(start).saturating_add(1))
        .collect()
}

/// Remove up to `limit` elements equal to `value`, from the front or the back,
/// and answer how many went.
fn remove(items: &mut Vec<Value>, value: &Value, limit: Option<usize>, from_back: bool) -> usize {
    let limit = limit.unwrap_or(usize::MAX);
    let mut removed = 0_usize;
    let mut keep = |item: &Value| {
        if removed < limit && item == value {
            removed = removed.saturating_add(1);
            false
        } else {
            true
        }
    };
    if from_back {
        let mut reversed: Vec<Value> = items.drain(..).rev().filter(|item| keep(item)).collect();
        reversed.reverse();
        *items = reversed;
    } else {
        items.retain(|item| keep(item));
    }
    removed
}
