//! `HASH SET | REMOVE | INCR`: fields of an object under one key.
//!
//! A field overwritten or removed loses its instant (ADR-0132); one altered in
//! place by `INCR`, or left alone by `IF ABSENT`, keeps it — the rule the cache
//! protocol's field expiry keeps.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use tessari_ql::{ArithmeticOp, Container, RecordTarget, Span, StructureOp};
use tessari_storage::Transaction;
use tessari_types::{Number, Value};

use super::counted;
use crate::arithmetic::arithmetic;
use crate::error::Result;
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// One `HASH` statement.
    pub(super) async fn hash(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        op: &StructureOp,
        span: Span,
    ) -> Result<Outcome> {
        if let Some(answer) = self.field_instants(transaction, target, op).await? {
            return Ok(answer);
        }
        let mut held = self.held(transaction, target, Container::Hash).await?;
        let mut fields = match &held.value {
            Value::Object(fields) => fields.clone(),
            _ => BTreeMap::new(),
        };
        let answer = match op {
            StructureOp::HashSet {
                fields: written,
                if_absent,
            } => {
                let mut added = 0_usize;
                for (name, value) in written {
                    let name = self.name(transaction, name, "field").await?;
                    let value = self.evaluate(transaction, value).await?;
                    match fields.entry(name) {
                        Entry::Occupied(mut field) => {
                            if !*if_absent {
                                held.field_expiry.retain(|(named, _)| named != field.key());
                                field.insert(value);
                            }
                        }
                        Entry::Vacant(free) => {
                            free.insert(value);
                            added = added.saturating_add(1);
                        }
                    }
                }
                if *if_absent && added == 0 {
                    return Ok(Outcome::Value(counted(0)));
                }
                counted(added)
            }
            StructureOp::HashRemove { fields: named } => {
                let mut removed = 0_usize;
                for name in named {
                    let name = self.name(transaction, name, "field").await?;
                    if fields.remove(&name).is_some() {
                        removed = removed.saturating_add(1);
                    }
                }
                if removed == 0 {
                    return Ok(Outcome::Value(counted(0)));
                }
                counted(removed)
            }
            StructureOp::HashIncr { field, by } => {
                let name = self.name(transaction, field, "field").await?;
                let by = self.evaluate(transaction, by).await?;
                let current = fields
                    .get(&name)
                    .cloned()
                    .unwrap_or(Value::Number(Number::Integer(0)));
                let next = arithmetic(ArithmeticOp::Add, &current, &by, span)?;
                fields.insert(name, next.clone());
                next
            }
            _ => return Ok(Outcome::Done),
        };
        let next = (!fields.is_empty()).then_some(Value::Object(fields));
        self.keep(transaction, held, next, span).await?;
        Ok(Outcome::Value(answer))
    }
}
