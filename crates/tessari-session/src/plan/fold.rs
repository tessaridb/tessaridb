use tessari_ql::{Expr, ExprKind, Projected};
use tessari_storage::Transaction;

use crate::error::Result;
use crate::session::Session;

use super::reads::reads_a_record;

/// Evaluate the parts of an expression that do not depend on a record, once.
///
/// # Why this is here and not in the evaluator
///
/// It is the same judgement the index selection makes — [`reads_a_record`]
/// decides whether a filter's right-hand side can be an index bound, and it
/// answers exactly the question a fold asks. Two notions of "constant" in one
/// query engine is one more than can be kept in step.
///
/// # What it was worth
///
/// The benchmark harness measured a nearest-neighbour read over 2000 records at
/// 12.5 ms and decomposed it: sorting the same records by a plain path costs
/// 1.5 ms, and the same distance with a **one**-component query vector costs
/// 2.3 ms. The cost tracked the size of the literal rather than the arithmetic
/// done on it, because a `[…32 numbers]` written once in the statement was being
/// rebuilt from the syntax tree for every record — 2000 fresh arrays of 32
/// values to re-create something that never changed.
///
/// # It cannot change an answer, with one exception that improves one
///
/// A constant folds to the value it already evaluated to. The exception is
/// `time::now()`, which was evaluated per record — so one statement could
/// observe two instants and sort by them. Folded, one statement observes one
/// instant, which is what a read should mean.
impl Session<'_> {
    /// This expression with its record-independent parts already evaluated.
    pub(crate) fn folded(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Expr> {
        if !reads_a_record(expr) {
            // Already a literal: folding would rebuild an identical node and
            // lose nothing but time.
            if matches!(expr.kind, ExprKind::Literal(_)) {
                return Ok(expr.clone());
            }
            let held = self.evaluate(transaction, expr)?;
            return Ok(Expr {
                // The original span, so an error still points where the author
                // wrote rather than where the fold put it.
                span: expr.span,
                kind: ExprKind::Literal(held),
            });
        }
        // It reads the record, so only its children can be constant.
        let kind = match &expr.kind {
            ExprKind::Not(inner) => ExprKind::Not(self.boxed(transaction, inner)?),
            ExprKind::Negate(inner) => ExprKind::Negate(self.boxed(transaction, inner)?),
            ExprKind::And(left, right) => ExprKind::And(
                self.boxed(transaction, left)?,
                self.boxed(transaction, right)?,
            ),
            ExprKind::Or(left, right) => ExprKind::Or(
                self.boxed(transaction, left)?,
                self.boxed(transaction, right)?,
            ),
            ExprKind::Binary { op, left, right } => ExprKind::Binary {
                op: *op,
                left: self.boxed(transaction, left)?,
                right: self.boxed(transaction, right)?,
            },
            ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
                op: *op,
                left: self.boxed(transaction, left)?,
                right: self.boxed(transaction, right)?,
            },
            ExprKind::Call {
                function,
                arguments,
                span,
            } => ExprKind::Call {
                function: *function,
                arguments: self.each(transaction, arguments)?,
                span: *span,
            },
            ExprKind::Array(items) => ExprKind::Array(self.each(transaction, items)?),
            // A set and an object are left alone. Both are built from their
            // items the way an array is, and neither appears in the positions
            // this pass exists for; folding them would be reach for its own sake.
            _ => return Ok(expr.clone()),
        };
        Ok(Expr {
            kind,
            span: expr.span,
        })
    }

    /// The same fold applied to each expression a projection evaluates.
    ///
    /// A fold is left alone: it answers after the per-record pass has finished,
    /// so it never reaches the loop this exists to relieve.
    pub(crate) fn folded_projection(
        &self,
        transaction: &mut Transaction<'_>,
        wanted: &[Projected],
    ) -> Result<Vec<Projected>> {
        let mut held = Vec::with_capacity(wanted.len());
        for projected in wanted {
            held.push(Projected {
                value: self.folded(transaction, &projected.value)?,
                name: projected.name.clone(),
            });
        }
        Ok(held)
    }

    /// The same fold applied to each key an order sorts by.
    ///
    /// One place rather than two, because both the streaming ordering stage and
    /// the one fed from a collected vector need it, and two copies would be two
    /// chances for a key to be folded differently from the record it is compared
    /// against.
    pub(crate) fn folded_order(
        &self,
        transaction: &mut Transaction<'_>,
        order: &[tessari_ql::Ordering],
    ) -> Result<Vec<Expr>> {
        let mut folded = Vec::with_capacity(order.len());
        for key in order {
            folded.push(self.folded(transaction, &key.key)?);
        }
        Ok(folded)
    }

    fn boxed(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Box<Expr>> {
        Ok(Box::new(self.folded(transaction, expr)?))
    }

    fn each(&self, transaction: &mut Transaction<'_>, items: &[Expr]) -> Result<Vec<Expr>> {
        items
            .iter()
            .map(|item| self.folded(transaction, item))
            .collect()
    }
}
