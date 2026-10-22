//! `HASH EXPIRE`, `HASH TTL` and `HASH PERSIST`: a hash field that stops being
//! answered at its own instant (ADR-0132).
//!
//! The instant lives in the record version beside the key's own, so every
//! read of the key leaves a passed field out — and a hash whose every field
//! passed reads as no key at all. Answers follow the cache protocol's codes,
//! one per field named: `-2` no such field, `0` the condition unmet, `1` done,
//! `2` removed because the instant is not in the future; `HASH TTL` answers
//! this store's own absences instead — `NONE` for no field, `NULL` for one
//! that never expires.

use tessari_encoding::FormatVersion;
use tessari_ql::{Container, Expr, FieldExpiryCondition, RecordTarget, StructureOp};
use tessari_storage::Transaction;
use tessari_types::{Number, Value};

use super::Held;
use crate::error::{Error, Result};
use crate::kv::expiry::{Instant, duration_left};
use crate::outcome::Outcome;
use crate::session::Session;

/// One field's answer as a value.
fn code(value: i64) -> Value {
    Value::Number(Number::Integer(value))
}

/// The instant `field` carries in `held`, if it carries one.
fn instant_of(held: &Held, field: &str) -> Option<u64> {
    held.field_expiry
        .iter()
        .find(|(named, _)| named == field)
        .map(|(_, at)| *at)
}

/// Whether a field holding `current` may be given `at` under `condition`.
const fn admits(condition: Option<FieldExpiryCondition>, current: Option<u64>, at: u64) -> bool {
    match (condition, current) {
        (None, _) | (Some(FieldExpiryCondition::None), None) => true,
        (Some(FieldExpiryCondition::Some), Some(_)) => true,
        // No instant is later than every instant.
        (Some(FieldExpiryCondition::Later), Some(held)) => at > held,
        (Some(FieldExpiryCondition::Sooner), Some(held)) => at < held,
        (Some(FieldExpiryCondition::Sooner), None) => true,
        _ => false,
    }
}

impl Session<'_> {
    /// Run `op` when it is one of the three, answering what it answers.
    pub(super) async fn field_instants(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        op: &StructureOp,
    ) -> Result<Option<Outcome>> {
        let answer = match op {
            StructureOp::HashExpire {
                fields,
                at,
                condition,
            } => {
                self.expire_fields(transaction, target, fields, at, *condition)
                    .await?
            }
            StructureOp::HashTtl { fields } => {
                self.fields_left(transaction, target, fields).await?
            }
            StructureOp::HashPersist { fields } => {
                self.persist_fields(transaction, target, fields).await?
            }
            _ => return Ok(None),
        };
        Ok(Some(Outcome::Value(Value::Array(answer))))
    }

    /// The field names `exprs` evaluate to.
    async fn field_names(
        &self,
        transaction: &mut Transaction<'_>,
        exprs: &[Expr],
    ) -> Result<Vec<String>> {
        let mut names = Vec::with_capacity(exprs.len());
        for expr in exprs {
            names.push(self.name(transaction, expr, "field").await?);
        }
        Ok(names)
    }

    async fn expire_fields(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        exprs: &[Expr],
        at: &Expr,
        condition: Option<FieldExpiryCondition>,
    ) -> Result<Vec<Value>> {
        self.refuse_a_format_the_store_does_not_hold(
            "a hash field's own expiry",
            FormatVersion::FIELD_EXPIRY,
        )
        .await?;
        let when = self.instant(transaction, at).await?;
        let names = self.field_names(transaction, exprs).await?;
        let mut held = self.held(transaction, target, Container::Hash).await?;
        let mut fields = match &held.value {
            Value::Object(fields) => fields.clone(),
            _ => return Ok(names.iter().map(|_| code(-2)).collect()),
        };
        let mut changed = false;
        let mut answer = Vec::with_capacity(names.len());
        for name in names {
            if !fields.contains_key(&name) {
                answer.push(code(-2));
                continue;
            }
            let current = instant_of(&held, &name);
            // A passed instant is judged as the earliest there is.
            let tested = match when {
                Instant::Future(at) => at,
                Instant::Passed => 0,
            };
            if !admits(condition, current, tested) {
                answer.push(code(0));
                continue;
            }
            held.field_expiry.retain(|(named, _)| *named != name);
            changed = true;
            match when {
                Instant::Passed => {
                    fields.remove(&name);
                    answer.push(code(2));
                }
                Instant::Future(at) => {
                    held.field_expiry.push((name, at));
                    answer.push(code(1));
                }
            }
        }
        if changed {
            let next = (!fields.is_empty()).then_some(Value::Object(fields));
            self.keep(transaction, held, next, at.span).await?;
        }
        Ok(answer)
    }

    /// `HASH TTL`, a read: the key's table is resolved but never written.
    async fn fields_left(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        exprs: &[Expr],
    ) -> Result<Vec<Value>> {
        let names = self.field_names(transaction, exprs).await?;
        let (_, address) = self.address(transaction, target).await?;
        let value = self.read_key(transaction, target).await?;
        let fields = match value {
            Value::Object(fields) => fields,
            Value::None => return Ok(names.iter().map(|_| Value::None).collect()),
            other => {
                return Err(Error::KeyHoldsAnotherKind {
                    key: address.id.to_string(),
                    holds: super::kind_of(&other),
                    wanted: Container::Hash.name(),
                    span: target.span,
                });
            }
        };
        let instants = transaction.field_expiry(&address).await?;
        let now = transaction.clock();
        Ok(names
            .iter()
            .map(|name| {
                if !fields.contains_key(name) {
                    return Value::None;
                }
                instants
                    .iter()
                    .find(|(named, _)| named == name)
                    .map_or(Value::Null, |(_, at)| duration_left(*at, now))
            })
            .collect())
    }

    async fn persist_fields(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        exprs: &[Expr],
    ) -> Result<Vec<Value>> {
        let names = self.field_names(transaction, exprs).await?;
        let mut held = self.held(transaction, target, Container::Hash).await?;
        let Value::Object(fields) = held.value.clone() else {
            return Ok(names.iter().map(|_| code(-2)).collect());
        };
        let mut changed = false;
        let mut answer = Vec::with_capacity(names.len());
        for name in names {
            if !fields.contains_key(&name) {
                answer.push(code(-2));
            } else if instant_of(&held, &name).is_some() {
                held.field_expiry.retain(|(named, _)| *named != name);
                changed = true;
                answer.push(code(1));
            } else {
                answer.push(code(-1));
            }
        }
        if changed {
            self.keep(transaction, held, Some(Value::Object(fields)), target.span)
                .await?;
        }
        Ok(answer)
    }
}
