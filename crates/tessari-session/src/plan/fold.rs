use tessari_ql::{Expr, ExprKind, Projected};
use tessari_storage::Transaction;

use crate::error::Result;
use crate::session::Session;

use super::reads::{answers_afresh, reads_a_record};

/// Evaluate the parts of an expression that do not depend on a record, once.
///
/// # Why this is here and not in the evaluator
///
/// It shares its first question with the index selection — [`reads_a_record`]
/// decides whether a filter's right-hand side can be an index bound. Two notions
/// of "constant" in one query engine is one more than can be kept in step, so
/// the walks live together in `super::reads`.
///
/// # Reading no record is not the same as being safe to evaluate once
///
/// It was treated as the same question until `rand::uuid()` arrived, and for the
/// 29 functions that came before, the two answers coincided. They part exactly
/// here: a generator reads no record, so the older condition called it constant
/// and `SELECT rand::uuid() AS id FROM users` wrote **one** identifier into every
/// row — no error, no failing test, and nothing visible until two records that
/// should differ do not. [`answers_afresh`] is the second half of the condition,
/// and `Function::purity` is what it consults.
///
/// The reverse mistake has the same shape and is why the second half is a
/// separate question rather than "impure does not fold": `time::now()` is impure
/// too and *must* fold, or one statement observes several instants and
/// `ORDER BY time::now()` sorts by a key regenerated under its own comparator.
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
    pub(crate) async fn folded(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
    ) -> Result<Expr> {
        if !reads_a_record(expr) && !answers_afresh(expr) {
            // Already a literal: folding would rebuild an identical node and
            // lose nothing but time.
            if matches!(expr.kind, ExprKind::Literal(_)) {
                return Ok(expr.clone());
            }
            let held = self.evaluate(transaction, expr).await?;
            return Ok(Expr {
                // The original span, so an error still points where the author
                // wrote rather than where the fold put it.
                span: expr.span,
                kind: ExprKind::Literal(held),
            });
        }
        // It reads the record, so only its children can be constant.
        let kind = match &expr.kind {
            ExprKind::Not(inner) => ExprKind::Not(self.boxed(transaction, inner).await?),
            ExprKind::Negate(inner) => ExprKind::Negate(self.boxed(transaction, inner).await?),
            ExprKind::And(left, right) => ExprKind::And(
                self.boxed(transaction, left).await?,
                self.boxed(transaction, right).await?,
            ),
            ExprKind::Or(left, right) => ExprKind::Or(
                self.boxed(transaction, left).await?,
                self.boxed(transaction, right).await?,
            ),
            ExprKind::Binary { op, left, right } => ExprKind::Binary {
                op: *op,
                left: self.boxed(transaction, left).await?,
                right: self.boxed(transaction, right).await?,
            },
            ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
                op: *op,
                left: self.boxed(transaction, left).await?,
                right: self.boxed(transaction, right).await?,
            },
            ExprKind::Call {
                function,
                arguments,
                span,
            } => ExprKind::Call {
                function: *function,
                arguments: self.each(transaction, arguments).await?,
                span: *span,
            },
            ExprKind::Array(items) => ExprKind::Array(self.each(transaction, items).await?),
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
    pub(crate) async fn folded_projection(
        &self,
        transaction: &mut Transaction<'_>,
        wanted: &[Projected],
    ) -> Result<Vec<Projected>> {
        let mut held = Vec::with_capacity(wanted.len());
        for projected in wanted {
            held.push(Projected {
                value: self.folded(transaction, &projected.value).await?,
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
    pub(crate) async fn folded_order(
        &self,
        transaction: &mut Transaction<'_>,
        order: &[tessari_ql::Ordering],
    ) -> Result<Vec<Expr>> {
        let mut folded = Vec::with_capacity(order.len());
        for key in order {
            folded.push(self.folded(transaction, &key.key).await?);
        }
        Ok(folded)
    }

    async fn boxed(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Box<Expr>> {
        // Folding recurses through here: the boxed future is what lets it.
        Ok(Box::new(Box::pin(self.folded(transaction, expr)).await?))
    }

    async fn each(&self, transaction: &mut Transaction<'_>, items: &[Expr]) -> Result<Vec<Expr>> {
        let mut folded = Vec::with_capacity(items.len());
        for item in items {
            folded.push(Box::pin(self.folded(transaction, item)).await?);
        }
        Ok(folded)
    }
}
