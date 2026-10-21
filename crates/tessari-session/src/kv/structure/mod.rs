//! `LIST`, `HASH`, `MEMBERS` and `SCORES`: the writes that change a container
//! held under one space key (G079 C6).
//!
//! # Atomic the way `INCR` is
//!
//! Each statement reads the key and writes it in its own transaction. Two
//! writers of one version are a write-write conflict the first committer wins,
//! and a lone statement that loses is run again on a fresh snapshot
//! ([`super::retried_on_conflict`]) — so a neighbour's push or increment is
//! never lost, and never handed back as a refusal either. The container is one
//! value, so a write re-encodes all of it (Q-G079-10).
//!
//! # What a key holds
//!
//! A list is an array, a hash an object and a set of members a set. Members
//! ranked by score are a set of `[score, member]` pairs, the score a float and
//! the member text — so the set's own order is the ranking, lowest first, and a
//! set of plain members never holds such a pair because a member is never an
//! array. A container a removal empties is deleted, so no key holds an empty
//! one, and a write keeps the expiry the key had.

mod hash;
mod list;
mod members;
mod scores;

use tessari_ql::{Container, Expr, RecordTarget, Span, StructureOp};
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{Number, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// The key a statement writes, as it stands before the write.
pub(super) struct Held {
    /// Where it lives.
    address: RecordAddress,
    /// The container, or `None` when there is no key.
    pub(super) value: Value,
    /// The instant it expires, kept across the write.
    expires: Option<u64>,
}

impl Session<'_> {
    /// Run one container write and answer what it answers.
    pub(crate) async fn structure(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        op: &StructureOp,
        span: Span,
    ) -> Result<Outcome> {
        match op.container() {
            Container::List => self.list(transaction, target, op, span).await,
            Container::Hash => self.hash(transaction, target, op, span).await,
            Container::Members => self.members(transaction, target, op, span).await,
            Container::Scores => self.scores(transaction, target, op, span).await,
        }
    }

    /// The key `target` names, refused when it holds another kind.
    pub(super) async fn held(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        wanted: Container,
    ) -> Result<Held> {
        let (_, address) = self.writable(transaction, target).await?;
        let value = self.read_key(transaction, target).await?;
        let holds = kind_of(&value);
        if value != Value::None && holds != wanted.name() {
            return Err(Error::KeyHoldsAnotherKind {
                key: address.id.to_string(),
                holds,
                wanted: wanted.name(),
                span: target.span,
            });
        }
        let expires = transaction.expires(&address).await?;
        Ok(Held {
            address,
            value,
            expires,
        })
    }

    /// Write `next` under the key `held` read, keeping its expiry — or remove
    /// the key when `next` is `None`, the container emptied.
    pub(super) async fn keep(
        &self,
        transaction: &mut Transaction<'_>,
        held: Held,
        next: Option<Value>,
        span: Span,
    ) -> Result<()> {
        match next {
            Some(value) => {
                self.put_record(transaction, held.address.clone(), value, span)
                    .await?;
                if let Some(at) = held.expires {
                    transaction.expire_pending(&held.address, at);
                }
                Ok(())
            }
            None if held.value != Value::None => {
                self.delete_record(transaction, held.address, span).await
            }
            None => Ok(()),
        }
    }

    /// Every expression in `exprs`, evaluated in order.
    pub(super) async fn evaluated(
        &self,
        transaction: &mut Transaction<'_>,
        exprs: &[Expr],
    ) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(exprs.len());
        for expr in exprs {
            values.push(self.evaluate(transaction, expr).await?);
        }
        Ok(values)
    }

    /// A whole number `expr` evaluates to.
    pub(super) async fn whole_number(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
        what: &'static str,
    ) -> Result<i64> {
        match self.evaluate(transaction, expr).await? {
            Value::Number(Number::Integer(value)) => Ok(value),
            _ => Err(Error::NotAWholeNumber {
                what,
                span: expr.span,
            }),
        }
    }

    /// A limit `expr` evaluates to — a whole number of zero or more.
    pub(super) async fn limit(
        &self,
        transaction: &mut Transaction<'_>,
        limit: Option<&Expr>,
    ) -> Result<Option<usize>> {
        let Some(limit) = limit else {
            return Ok(None);
        };
        let count = self.whole_number(transaction, limit, "limit").await?;
        usize::try_from(count)
            .map(Some)
            .map_err(|_| Error::NotAWholeNumber {
                what: "limit",
                span: limit.span,
            })
    }

    /// The text a field or member name evaluates to.
    pub(super) async fn name(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
        what: &'static str,
    ) -> Result<String> {
        match self.evaluate(transaction, expr).await? {
            Value::String(text) => Ok(text),
            other => Err(Error::NameNotText {
                what,
                found: other.type_name(),
                span: expr.span,
            }),
        }
    }
}

/// The kind a held value is, as a refusal names it.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Array(_) => Container::List.name(),
        Value::Set(members) if is_ranking(members) => Container::Scores.name(),
        Value::Set(_) => Container::Members.name(),
        Value::Object(_) => Container::Hash.name(),
        Value::String(_) | Value::Bytes(_) => "string",
        Value::Number(_) => "number",
        _ => "value",
    }
}

/// Whether a set is members and their scores: not empty, every element a
/// `[float, text]` pair.
pub(super) fn is_ranking(members: &std::collections::BTreeSet<Value>) -> bool {
    !members.is_empty() && members.iter().all(|member| {
        matches!(
            member,
            Value::Array(pair)
                if matches!(pair.as_slice(), [Value::Number(Number::Float(_)), Value::String(_)])
        )
    })
}

/// A count as the integer a statement answers.
pub(super) fn counted(count: usize) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}
