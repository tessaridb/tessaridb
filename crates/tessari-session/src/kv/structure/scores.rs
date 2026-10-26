//! `SCORES ADD | INCR | REMOVE | POP`: members ranked by score under one key.
//!
//! Held as a set of `[score, member]` pairs, so the set's own order — score,
//! then member — is the ranking, and the lowest is its first element.

use std::collections::BTreeSet;

use tessari_ql::{
    Container, Expr, Presence, RangeEnd, RecordTarget, ScoreCompare, ScoreRange, Span, StructureOp,
};
use tessari_storage::Transaction;
use tessari_types::{Number, Value};

use super::counted;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// One `SCORES` statement.
    pub(super) async fn scores(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        op: &StructureOp,
        span: Span,
    ) -> Result<Outcome> {
        let held = self.held(transaction, target, Container::Scores).await?;
        let mut ranked = match &held.value {
            Value::Set(pairs) => pairs.clone(),
            _ => BTreeSet::new(),
        };
        let answer = match op {
            StructureOp::ScoresAdd {
                scores,
                presence,
                compare,
                changed,
            } => {
                let (mut added, mut moved) = (0_usize, 0_usize);
                for (member, score) in scores {
                    let member = self.name(transaction, member, "member").await?;
                    let score = self.score(transaction, score).await?;
                    let had = score_of(&ranked, &member);
                    let wanted = match (had, presence, compare) {
                        (Some(_), Some(Presence::Absent), _)
                        | (None, Some(Presence::Present), _) => false,
                        (Some(old), _, Some(ScoreCompare::Greater)) => score > old,
                        (Some(old), _, Some(ScoreCompare::Less)) => score < old,
                        (Some(old), _, None) => score != old,
                        (None, _, _) => true,
                    };
                    if !wanted {
                        continue;
                    }
                    match had {
                        Some(old) => {
                            ranked.remove(&pair(old, &member));
                            moved = moved.saturating_add(1);
                        }
                        None => added = added.saturating_add(1),
                    }
                    ranked.insert(pair(score, &member));
                }
                let answered = if *changed {
                    added.saturating_add(moved)
                } else {
                    added
                };
                if added == 0 && moved == 0 {
                    return Ok(Outcome::Value(counted(0)));
                }
                counted(answered)
            }
            StructureOp::ScoresIncr { member, by } => {
                let member = self.name(transaction, member, "member").await?;
                let amount = self.score(transaction, by).await?;
                let old = score_of(&ranked, &member);
                let score = old.unwrap_or(0.0) + amount;
                if !score.is_finite() {
                    return Err(Error::ScoreNotANumber {
                        found: "a score out of range",
                        span: by.span,
                    });
                }
                if let Some(old) = old {
                    ranked.remove(&pair(old, &member));
                }
                ranked.insert(pair(score, &member));
                Value::Number(Number::float(score))
            }
            StructureOp::ScoresRemove { members } => {
                let mut removed = 0_usize;
                for member in members {
                    let member = self.name(transaction, member, "member").await?;
                    if let Some(old) = score_of(&ranked, &member) {
                        ranked.remove(&pair(old, &member));
                        removed = removed.saturating_add(1);
                    }
                }
                if removed == 0 {
                    return Ok(Outcome::Value(counted(0)));
                }
                counted(removed)
            }
            StructureOp::ScoresRemoveRange { range } => {
                let named = self.ranged(transaction, &ranked, range).await?;
                if named.is_empty() {
                    return Ok(Outcome::Value(counted(0)));
                }
                for held in &named {
                    ranked.remove(held);
                }
                counted(named.len())
            }
            StructureOp::ScoresPop { highest, limit } => {
                let limit = self.limit(transaction, limit.as_ref()).await?;
                let mut taken = Vec::new();
                while taken.len() < limit.unwrap_or(1) {
                    let next = if *highest {
                        ranked.pop_last()
                    } else {
                        ranked.pop_first()
                    };
                    let Some(Value::Array(pair)) = next else {
                        break;
                    };
                    taken.push(Value::Array(pair.into_iter().rev().collect()));
                }
                if taken.is_empty() {
                    return Ok(Outcome::Value(Value::Array(Vec::new())));
                }
                Value::Array(taken)
            }
            _ => return Ok(Outcome::Done),
        };
        let next = (!ranked.is_empty()).then_some(Value::Set(ranked));
        self.keep(transaction, held, next, span).await?;
        Ok(Outcome::Value(answer))
    }

    /// The held pairs `range` names, lowest first.
    async fn ranged(
        &self,
        transaction: &mut Transaction<'_>,
        ranked: &BTreeSet<Value>,
        range: &ScoreRange,
    ) -> Result<Vec<Value>> {
        let held = ranked.iter().filter_map(|held| match held {
            Value::Array(pair) => match pair.as_slice() {
                [Value::Number(Number::Float(score)), Value::String(member)] => {
                    Some((held, *score, member.as_str()))
                }
                _ => None,
            },
            _ => None,
        });
        Ok(match range {
            ScoreRange::Rank { start, stop } => {
                let start = self.whole_number(transaction, start, "position").await?;
                let stop = self.whole_number(transaction, stop, "position").await?;
                let Some((from, to)) = span_of(ranked.len(), start, stop) else {
                    return Ok(Vec::new());
                };
                held.skip(from)
                    .take(to.saturating_sub(from).saturating_add(1))
                    .map(|(pair, _, _)| pair.clone())
                    .collect()
            }
            ScoreRange::Score { low, high } => {
                let low = self.score_end(transaction, low).await?;
                let high = self.score_end(transaction, high).await?;
                held.filter(|(_, score, _)| {
                    low.is_none_or(|(at, out)| if out { *score > at } else { *score >= at })
                        && high.is_none_or(|(at, out)| if out { *score < at } else { *score <= at })
                })
                .map(|(pair, _, _)| pair.clone())
                .collect()
            }
            ScoreRange::Member { low, high } => {
                let low = self.member_end(transaction, low).await?;
                let high = self.member_end(transaction, high).await?;
                held.filter(|(_, _, member)| {
                    low.as_ref().is_none_or(|(at, out)| {
                        if *out {
                            *member > at.as_str()
                        } else {
                            *member >= at.as_str()
                        }
                    }) && high.as_ref().is_none_or(|(at, out)| {
                        if *out {
                            *member < at.as_str()
                        } else {
                            *member <= at.as_str()
                        }
                    })
                })
                .map(|(pair, _, _)| pair.clone())
                .collect()
            }
        })
    }

    /// A score end, an end evaluating to `NONE` being no end.
    async fn score_end(
        &self,
        transaction: &mut Transaction<'_>,
        end: &RangeEnd,
    ) -> Result<Option<(f64, bool)>> {
        Ok(match self.evaluate(transaction, &end.value).await? {
            Value::None => None,
            value => Some((finite(&value, end.value.span)?, end.exclusive)),
        })
    }

    /// A member end, an end evaluating to `NONE` being no end.
    async fn member_end(
        &self,
        transaction: &mut Transaction<'_>,
        end: &RangeEnd,
    ) -> Result<Option<(String, bool)>> {
        Ok(match self.evaluate(transaction, &end.value).await? {
            Value::None => None,
            Value::String(member) => Some((member, end.exclusive)),
            other => {
                return Err(Error::NameNotText {
                    what: "member",
                    found: other.type_name(),
                    span: end.value.span,
                });
            }
        })
    }

    /// A finite score `expr` evaluates to.
    async fn score(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<f64> {
        let value = self.evaluate(transaction, expr).await?;
        finite(&value, expr.span)
    }
}

/// The finite score `value` is, refused naming what it is otherwise.
fn finite(value: &Value, span: Span) -> Result<f64> {
    match value {
        Value::Number(number) => {
            number
                .as_float()
                .filter(|score| score.is_finite())
                .ok_or(Error::ScoreNotANumber {
                    found: "a number no float holds",
                    span,
                })
        }
        other => Err(Error::ScoreNotANumber {
            found: other.type_name(),
            span,
        }),
    }
}

/// The positions from `start` to `stop` of a ranking `length` long, both
/// kept, either counted from the highest when negative; `None` when that
/// names nothing.
pub(crate) fn span_of(length: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    let length = i64::try_from(length).ok()?;
    let from_top = |index: i64| {
        if index < 0 {
            length.saturating_add(index)
        } else {
            index
        }
    };
    let start = from_top(start).max(0);
    let stop = from_top(stop).min(length.saturating_sub(1));
    if start > stop {
        return None;
    }
    Some((usize::try_from(start).ok()?, usize::try_from(stop).ok()?))
}

/// The pair a member and its score are held as.
fn pair(score: f64, member: &str) -> Value {
    Value::Array(vec![
        Value::Number(Number::float(score)),
        Value::from(member),
    ])
}

/// The score `member` holds, if it is there.
fn score_of(ranked: &BTreeSet<Value>, member: &str) -> Option<f64> {
    ranked.iter().find_map(|held| match held {
        Value::Array(pair) => match pair.as_slice() {
            [Value::Number(Number::Float(score)), Value::String(name)] if name == member => {
                Some(*score)
            }
            _ => None,
        },
        _ => None,
    })
}
