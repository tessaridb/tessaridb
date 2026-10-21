//! `MEMBERS ADD | REMOVE | POP | MOVE`: a set of members under one key.
//!
//! A member is a single value — text, a number, bytes — and never a container,
//! so a set of members can never be read as a ranking (see the module above).

use std::collections::BTreeSet;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use tessari_ql::{Container, Expr, RecordTarget, Span, StructureOp};
use tessari_storage::Transaction;
use tessari_types::Value;

use super::counted;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// One `MEMBERS` statement.
    pub(super) async fn members(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        op: &StructureOp,
        span: Span,
    ) -> Result<Outcome> {
        let held = self.held(transaction, target, Container::Members).await?;
        let mut members = match &held.value {
            Value::Set(members) => members.clone(),
            _ => BTreeSet::new(),
        };
        let answer = match op {
            StructureOp::MembersAdd { values } => {
                let mut added = 0_usize;
                for value in values {
                    if members.insert(self.member(transaction, value).await?) {
                        added = added.saturating_add(1);
                    }
                }
                if added == 0 {
                    return Ok(Outcome::Value(counted(0)));
                }
                counted(added)
            }
            StructureOp::MembersRemove { values } => {
                let mut removed = 0_usize;
                for value in values {
                    if members.remove(&self.evaluate(transaction, value).await?) {
                        removed = removed.saturating_add(1);
                    }
                }
                if removed == 0 {
                    return Ok(Outcome::Value(counted(0)));
                }
                counted(removed)
            }
            StructureOp::MembersPop { limit } => {
                let limit = self.limit(transaction, limit.as_ref()).await?;
                let mut taken = Vec::new();
                while taken.len() < limit.unwrap_or(1) {
                    let Some(chosen) = chosen(&members) else {
                        break;
                    };
                    members.remove(&chosen);
                    taken.push(chosen);
                }
                if taken.is_empty() {
                    return Ok(Outcome::Value(match limit {
                        Some(_) => Value::Array(Vec::new()),
                        None => Value::None,
                    }));
                }
                match limit {
                    Some(_) => Value::Array(taken),
                    None => taken.pop().unwrap_or(Value::None),
                }
            }
            StructureOp::MembersMove { value, to } => {
                let member = self.evaluate(transaction, value).await?;
                if !members.contains(&member) {
                    return Ok(Outcome::Value(Value::Bool(false)));
                }
                let (_, destination) = self.writable(transaction, to).await?;
                if destination == held.address {
                    return Ok(Outcome::Value(Value::Bool(true)));
                }
                let other = self.held(transaction, to, Container::Members).await?;
                let mut theirs = match &other.value {
                    Value::Set(members) => members.clone(),
                    _ => BTreeSet::new(),
                };
                members.remove(&member);
                theirs.insert(member);
                self.keep(transaction, other, Some(Value::Set(theirs)), span)
                    .await?;
                Value::Bool(true)
            }
            _ => return Ok(Outcome::Done),
        };
        let next = (!members.is_empty()).then_some(Value::Set(members));
        self.keep(transaction, held, next, span).await?;
        Ok(Outcome::Value(answer))
    }

    /// A member `expr` evaluates to, refused when it is a container.
    async fn member(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Value> {
        let value = self.evaluate(transaction, expr).await?;
        if matches!(
            value,
            Value::Array(_) | Value::Object(_) | Value::Set(_) | Value::None
        ) {
            return Err(Error::MemberIsAContainer {
                found: value.type_name(),
                span: expr.span,
            });
        }
        Ok(value)
    }
}

/// One member chosen at random, or none from an empty set.
fn chosen(members: &BTreeSet<Value>) -> Option<Value> {
    let count = u64::try_from(members.len())
        .ok()
        .filter(|count| *count > 0)?;
    let at = usize::try_from(OsRng.next_u64().checked_rem(count)?).ok()?;
    members.iter().nth(at).cloned()
}
