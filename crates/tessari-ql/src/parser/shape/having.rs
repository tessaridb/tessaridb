//! `… GROUP BY … HAVING <condition>` (ADR-0134 D5).
//!
//! The condition is carried as one more projected value, under a name no
//! statement can write, so every path that folds a grouped read — on this node,
//! on the leaders of a split table, over a rollup — computes its folds exactly
//! as it computes the projection's. The read then keeps the rows it holds for
//! and removes the value before answering.

use crate::ast::{Expr, ExprKind, Name, Projected, Projection};
use crate::error::{Error, Result};
use crate::parser::Parser;
use crate::token::Span;

/// The name the condition is projected under: a space cannot begin a name the
/// language lets anybody write, so it can collide with nothing.
pub const HAVING_NAME: &str = " having";

impl Parser<'_> {
    /// `HAVING <condition>` after a `GROUP BY`, when it is there, joined to
    /// the projection; the span of the word when it was written.
    pub(in crate::parser) fn having(
        &mut self,
        projection: &mut Projection,
        (grouped, filled): (bool, bool),
    ) -> Result<Option<Span>> {
        let span = self.span_here();
        if !self.eat_word("having") {
            return Ok(None);
        }
        if !grouped {
            return Err(Error::UnexpectedToken {
                expected: "`GROUP BY` before `HAVING` — it filters groups, and a `WHERE` filters records",
                found: "`HAVING`".to_owned(),
                span,
            });
        }
        if filled {
            return Err(Error::UnexpectedToken {
                expected: "a read without `FILL` — `FILL` answers every window of its range, and a \
                           `HAVING` removing some would contradict it",
                found: "`HAVING`".to_owned(),
                span,
            });
        }
        let condition = self.condition()?;
        let Projection::Values { values, .. } = projection else {
            // `SELECT *` over a group is refused by the grouping rule.
            return Ok(Some(span));
        };
        let condition = named(&condition, values);
        values.push(Projected {
            value: condition,
            name: Name {
                text: HAVING_NAME.to_owned(),
                span,
            },
        });
        Ok(Some(span))
    }
}

/// The condition with each bare name a projection answers under replaced by
/// what that projection computes, so `HAVING n > 1` reads `count(*) AS n`.
fn named(expr: &Expr, values: &[Projected]) -> Expr {
    if let ExprKind::Path(field) = &expr.kind
        && field.path.steps().is_empty()
        && let Some(projected) = values
            .iter()
            .find(|value| value.name.text == field.path.root())
    {
        return projected.value.clone();
    }
    let kind = match &expr.kind {
        ExprKind::Not(inner) => ExprKind::Not(Box::new(named(inner, values))),
        ExprKind::Negate(inner) => ExprKind::Negate(Box::new(named(inner, values))),
        ExprKind::And(left, right) => ExprKind::And(
            Box::new(named(left, values)),
            Box::new(named(right, values)),
        ),
        ExprKind::Or(left, right) => ExprKind::Or(
            Box::new(named(left, values)),
            Box::new(named(right, values)),
        ),
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(named(left, values)),
            right: Box::new(named(right, values)),
        },
        ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
            op: *op,
            left: Box::new(named(left, values)),
            right: Box::new(named(right, values)),
        },
        other => other.clone(),
    };
    Expr {
        kind,
        span: expr.span,
    }
}
