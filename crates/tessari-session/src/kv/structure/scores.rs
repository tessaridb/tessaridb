//! `SCORES ADD | INCR | REMOVE | POP`: members ranked by score under one key.
//!
//! Held as a set of `[score, member]` pairs, so the set's own order — score,
//! then member — is the ranking, and the lowest is its first element.

use std::collections::BTreeSet;

use tessari_ql::{Container, Expr, Presence, RecordTarget, ScoreCompare, Span, StructureOp};
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

    /// A finite score `expr` evaluates to.
    async fn score(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<f64> {
        let value = self.evaluate(transaction, expr).await?;
        match &value {
            Value::Number(number) => {
                number
                    .as_float()
                    .filter(|score| score.is_finite())
                    .ok_or(Error::ScoreNotANumber {
                        found: "a number no float holds",
                        span: expr.span,
                    })
            }
            other => Err(Error::ScoreNotANumber {
                found: other.type_name(),
                span: expr.span,
            }),
        }
    }
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
