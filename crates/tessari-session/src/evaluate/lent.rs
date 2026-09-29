//! A distance computed from its operands where they stand.
//!
//! A nearest-neighbour read computes one distance per record, and evaluating
//! its two operands the ordinary way clones both — the record's whole vector
//! and the query's — only to read them once and drop them. Those two copies were
//! a quarter of an exact scan. A field and a literal are already values, so they
//! are lent instead; anything else is evaluated as usual.

use std::borrow::Cow;

use tessari_ql::{Expr, ExprKind, Function};
use tessari_storage::Transaction;
use tessari_types::Value;

use crate::error::{Error, Result};
use crate::session::Session;

use super::Scope;

impl Session<'_> {
    /// The distance between two operands, or `None` when this is not a distance
    /// taken over exactly two of them — the ordinary call answers those, with
    /// its own refusals.
    pub(super) fn distance_between(
        &self,
        transaction: &mut Transaction<'_>,
        function: Function,
        arguments: &[Expr],
        scope: Scope<'_>,
    ) -> Result<Option<Value>> {
        if !matches!(
            function,
            Function::VectorCosine | Function::VectorEuclidean | Function::VectorDot
        ) {
            return Ok(None);
        }
        let [left, right] = arguments else {
            return Ok(None);
        };
        let left = self.lent(transaction, left, scope)?;
        let right = self.lent(transaction, right, scope)?;
        // No absence check: a distance answers for an absent operand — it is
        // infinitely far — exactly as the ordinary call lets it.
        Ok(Some(crate::vector::distance(function, &left, &right)))
    }

    /// An operand as it stands when it is a field or a literal, and evaluated
    /// otherwise.
    fn lent<'a>(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &'a Expr,
        scope: Scope<'a>,
    ) -> Result<Cow<'a, Value>> {
        match &expr.kind {
            ExprKind::Path(field) => {
                let Some(record) = scope.record else {
                    return Err(Error::NoRecordInScope { span: field.span });
                };
                Ok(field
                    .path
                    .resolve(record)
                    .map_or(Cow::Owned(Value::None), Cow::Borrowed))
            }
            ExprKind::Literal(value) => Ok(Cow::Borrowed(value)),
            _ => self.evaluate_in(transaction, expr, scope).map(Cow::Owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use tessari_ql::{Function, Span};
    use tessari_types::{Number, Value};

    use crate::call::call;
    use crate::vector::distance;

    fn vector(components: &[f64]) -> Value {
        Value::Array(
            components
                .iter()
                .map(|held| Value::Number(Number::float(*held)))
                .collect(),
        )
    }

    /// Operands a distance may meet, the absences and the wrong kinds included.
    fn operands() -> Vec<Value> {
        vec![
            vector(&[1.0, 0.0, 0.5]),
            vector(&[0.0, 0.0, 0.0]),
            vector(&[1.0, 2.0]),
            vector(&[]),
            Value::Array(vec![
                Value::Number(Number::Integer(1)),
                Value::Number(Number::Decimal(rust_decimal::Decimal::new(25, 1))),
                Value::Number(Number::float(-3.0)),
            ]),
            Value::Array(vec![
                Value::from("one"),
                Value::from("two"),
                Value::from("three"),
            ]),
            Value::None,
            Value::Null,
            Value::from("text"),
            Value::Number(Number::Integer(3)),
        ]
    }

    #[test]
    fn lending_the_operands_answers_what_the_call_answers() {
        for function in [
            Function::VectorCosine,
            Function::VectorEuclidean,
            Function::VectorDot,
        ] {
            for left in operands() {
                for right in operands() {
                    let called = call(function, &[left.clone(), right.clone()], Span::new(0, 1))
                        .expect("a distance is answered, never refused, for two operands");
                    assert_eq!(
                        format!("{:?}", distance(function, &left, &right)),
                        format!("{called:?}"),
                        "{function:?} of {left:?} and {right:?}"
                    );
                }
            }
        }
    }
}
