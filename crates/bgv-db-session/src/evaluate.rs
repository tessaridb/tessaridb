//! Turning an expression into a value.
//!
//! Two of the forms are reads, and that is the whole of what makes the models
//! compose. A `GET` inside a record statement runs **in the same transaction**,
//! so it sees the same snapshot as the statement around it — two models that
//! cannot share a snapshot are two databases sharing a process.

use core::ops::Bound;
use std::collections::BTreeMap;

use bgv_db_encoding::decode_payload;
use bgv_db_ql::{Expr, ExprKind, RecordTarget, Select, Source, Span};
use bgv_db_storage::{RecordAddress, Transaction};
use bgv_db_types::{Number, RecordId, RecordRef, Value, ValueRange};

use crate::error::{Error, Result};
use crate::session::Session;

impl Session<'_> {
    /// The value an expression denotes.
    pub(crate) fn evaluate(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Value> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Table(table) => {
                let (_, id) = self.resolve_table(transaction, table)?;
                Ok(Value::Table(id))
            }
            ExprKind::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                Ok(Value::Record(RecordRef::new(address.table, address.id)))
            }
            ExprKind::Array(items) => Ok(Value::Array(self.values(transaction, items)?)),
            ExprKind::Set(items) => Ok(Value::Set(
                self.values(transaction, items)?.into_iter().collect(),
            )),
            ExprKind::Object(fields) => {
                let mut object = BTreeMap::new();
                for field in fields {
                    let value = self.evaluate(transaction, &field.value)?;
                    object.insert(field.name.text.clone(), value);
                }
                Ok(Value::Object(object))
            }
            ExprKind::Range(range) => {
                let start = self.evaluate(transaction, &range.start)?;
                let end = self.evaluate(transaction, &range.end)?;
                let end = if range.inclusive {
                    Bound::Included(end)
                } else {
                    Bound::Excluded(end)
                };
                Ok(Value::Range(Box::new(ValueRange::new(
                    Bound::Included(start),
                    end,
                ))))
            }
            ExprKind::Get(target) => self.read_key(transaction, target),
            ExprKind::Select(select) => self.read_as_value(transaction, select),
        }
    }

    fn values(&self, transaction: &mut Transaction<'_>, items: &[Expr]) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(self.evaluate(transaction, item)?);
        }
        Ok(values)
    }

    /// Where a record lives, once its table name is resolved.
    pub(crate) fn address(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<(crate::context::Context, RecordAddress)> {
        let (context, table) = self.resolve_table(transaction, &target.table)?;
        Ok((
            context,
            RecordAddress::new(
                context.namespace,
                context.database,
                table,
                target.id.clone(),
            ),
        ))
    }

    /// The value under a key, or [`Value::None`] when there is nothing there.
    pub(crate) fn read_key(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<Value> {
        let (_, address) = self.address(transaction, target)?;
        match transaction.get(&address)? {
            Some(payload) => Ok(decode_payload(&payload)?),
            None => Ok(Value::None),
        }
    }

    /// A read, as the records it found.
    pub(crate) fn read(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Vec<(RecordId, Value)>> {
        match &select.from {
            Source::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                match transaction.get(&address)? {
                    Some(payload) => Ok(vec![(address.id, decode_payload(&payload)?)]),
                    None => Ok(Vec::new()),
                }
            }
            Source::Table(table) => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let found = transaction.scan_table(context.namespace, context.database, id)?;
                decode_all(found)
            }
            Source::Index {
                table,
                field,
                value,
            } => {
                let (_, id) = self.resolve_table(transaction, table)?;
                let index = self.index_on_field(transaction, id, &field.text, field.span)?;
                let wanted = self.evaluate(transaction, value)?;
                let found = transaction.records_by_index(&index, &[wanted])?;
                decode_all(found)
            }
        }
    }

    /// A read standing where a value stands.
    ///
    /// One record answers with its own value; a read of several answers with an
    /// array, so that the shape of the answer follows the shape of the question
    /// rather than the number of rows that happened to match.
    fn read_as_value(&self, transaction: &mut Transaction<'_>, select: &Select) -> Result<Value> {
        let records = self.read(transaction, select)?;
        if matches!(select.from, Source::Record(_)) {
            return Ok(records
                .into_iter()
                .next()
                .map_or(Value::None, |(_, value)| value));
        }
        Ok(Value::Array(
            records.into_iter().map(|(_, value)| value).collect(),
        ))
    }
}

fn decode_all(found: Vec<(RecordId, Vec<u8>)>) -> Result<Vec<(RecordId, Value)>> {
    let mut records = Vec::with_capacity(found.len());
    for (id, payload) in found {
        records.push((id, decode_payload(&payload)?));
    }
    Ok(records)
}

/// The record identity a range bound names.
///
/// A key is a record id, so a bound has to be one of the four kinds an identity
/// has; anything else is a bound that could never match a key.
pub(crate) fn key_bound(value: &Value, span: Span) -> Result<RecordId> {
    match value {
        Value::Number(Number::Integer(id)) => Ok(RecordId::Int(*id)),
        Value::String(text) => Ok(RecordId::Text(text.clone())),
        Value::Uuid(bytes) => Ok(RecordId::Uuid(*bytes)),
        Value::Bytes(bytes) => Ok(RecordId::Bytes(bytes.clone())),
        _ => Err(Error::InvalidKeyBound { span }),
    }
}

/// Whether a key falls inside a bound pair.
pub(crate) fn within(id: &RecordId, start: &RecordId, end: &RecordId, inclusive: bool) -> bool {
    if id < start {
        return false;
    }
    if inclusive { id <= end } else { id < end }
}
