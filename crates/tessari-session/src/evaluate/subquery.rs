//! A subquery as an operand of a comparison (ADR-0134 D3, Amendment 2).
//!
//! A subquery anywhere else answers with its records, which is what it always
//! did. Beside a comparison with a single value — not an array, not a
//! document — and projecting exactly one named value, it answers what a SQL
//! reader means by it: the values for membership, the one value for an ordered
//! or equal comparison.

use tessari_ql::{Expr, ExprKind, Projection, Select, Source, Span};
use tessari_storage::Transaction;
use tessari_types::{BinaryOp, Value};

use crate::budget::Ceiling;
use crate::error::{Error, Result};
use crate::session::Session;

use super::{Answered, Scope};

/// Which side of a comparison an operand stands on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
}

/// An operand evaluated, and — when it was a subquery projecting one value —
/// that value of each record it answered.
pub(super) struct Operand {
    /// What the operand is anywhere else: a subquery's records as an array.
    pub(super) plain: Value,
    projected: Option<(Vec<Value>, Span)>,
}

impl Operand {
    /// The value the comparison uses, given what stands on the other side.
    ///
    /// `x IN (SELECT y …)` and `(SELECT y …) CONTAINS x` take the subquery's
    /// values; `=`, `!=`, the four orderings, `LIKE` and `ILIKE` take its one
    /// value, `NONE` when no record answered and refused when several did. A
    /// subquery compared with an array or a document keeps comparing its
    /// records, which is how `(SELECT n FROM t) = [{ n: 1 }]` reads.
    pub(super) fn against(self, (op, side): (BinaryOp, Side), other: &Value) -> Result<Value> {
        let Some((values, span)) = self.projected else {
            return Ok(self.plain);
        };
        if matches!(other, Value::Array(_) | Value::Object(_)) {
            return Ok(self.plain);
        }
        if matches!(
            (op, side),
            (BinaryOp::In, Side::Right) | (BinaryOp::Contains, Side::Left)
        ) {
            return Ok(Value::Array(values));
        }
        let single = matches!(
            op,
            BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessOrEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterOrEqual
                | BinaryOp::Like
                | BinaryOp::Ilike
        );
        if !single {
            return Ok(self.plain);
        }
        let mut values = values.into_iter();
        let first = values.next().unwrap_or(Value::None);
        let more = values.count();
        if more > 0 {
            return Err(Error::SubqueryNotOneValue {
                found: more.saturating_add(1),
                span,
            });
        }
        Ok(first)
    }
}

impl Session<'_> {
    /// One operand of a comparison, evaluated once; [`Operand::against`] then
    /// decides which reading of it the comparison takes.
    pub(super) async fn operand(
        &self,
        transaction: &mut Transaction<'_>,
        operand: &Expr,
        scope: Scope<'_>,
    ) -> Result<Operand> {
        let ExprKind::Select(select) = &operand.kind else {
            return Ok(Operand {
                plain: self.evaluate_child(transaction, operand, scope).await?,
                projected: None,
            });
        };
        let Some(name) = single_value(select) else {
            return Ok(Operand {
                plain: self.evaluate_child(transaction, operand, scope).await?,
                projected: None,
            });
        };
        // A read of one record already answers as that record, not an array.
        if select.only.is_some() || matches!(select.from, Source::Record(_) | Source::Node) {
            return Ok(Operand {
                plain: self.evaluate_child(transaction, operand, scope).await?,
                projected: None,
            });
        }
        let Answered { records, .. } =
            Box::pin(self.read(transaction, select, None, Ceiling::over(select))).await?;
        let values = records
            .iter()
            .map(|(_, record)| match record {
                Value::Object(fields) => fields.get(name).cloned().unwrap_or(Value::None),
                other => other.clone(),
            })
            .collect();
        Ok(Operand {
            plain: Value::Array(records.into_iter().map(|(_, record)| record).collect()),
            projected: Some((values, operand.span)),
        })
    }
}

/// The name of the one value a subquery projects, when it projects exactly one
/// and no `*`.
fn single_value(select: &Select) -> Option<&str> {
    match &select.projection {
        Projection::Values {
            everything: None,
            values,
        } => match values.as_slice() {
            [only] => Some(only.name.text.as_str()),
            _ => None,
        },
        Projection::All | Projection::Values { .. } => None,
    }
}
