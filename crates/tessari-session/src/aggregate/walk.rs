//! Walking an expression for its folds, and putting what they answered back.

use tessari_ql::{Expr, ExprKind};
use tessari_types::Value;

/// The expressions one expression is built out of.
pub(super) fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Fold { over, .. } => over.as_deref().into_iter().collect(),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => vec![inner],
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => vec![left, right],
        ExprKind::Call { arguments, .. } => arguments.iter().collect(),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().collect(),
        ExprKind::Object(fields) => fields.iter().map(|field| &field.value).collect(),
        ExprKind::Range(range) => vec![&range.start, &range.end],
        _ => Vec::new(),
    }
}

/// Every fold in this expression, in the order a walk meets them.
///
/// The **order is the identity**: what a fold collected and what it computed are
/// matched up by position, so the collecting walk and the substituting walk have
/// to meet the folds the same way. They do because both use this function, which
/// is why it exists rather than each walking the tree in its own words.
pub(super) fn folds_in<'a>(expr: &'a Expr, found: &mut Vec<&'a Expr>) {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        found.push(expr);
    }
    for child in children(expr) {
        folds_in(child, found);
    }
}

/// The same expression with each fold replaced by the value it produced.
///
/// A fold's value is **constant within its group**, and this makes that
/// literally true rather than a claim about the implementation: what the
/// ordinary evaluator then meets is arithmetic over a literal, and it needs to
/// know nothing about folding at all.
///
/// `taken` is consumed in walk order, which is the order [`folds_in`] produced
/// the collections in.
pub(super) fn substituted(expr: &Expr, taken: &mut std::vec::IntoIter<Value>) -> Expr {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        return Expr {
            kind: ExprKind::Literal(taken.next().unwrap_or(Value::None)),
            span: expr.span,
        };
    }
    let kind = match &expr.kind {
        ExprKind::Not(inner) => ExprKind::Not(Box::new(substituted(inner, taken))),
        ExprKind::Negate(inner) => ExprKind::Negate(Box::new(substituted(inner, taken))),
        ExprKind::And(left, right) => ExprKind::And(
            Box::new(substituted(left, taken)),
            Box::new(substituted(right, taken)),
        ),
        ExprKind::Or(left, right) => ExprKind::Or(
            Box::new(substituted(left, taken)),
            Box::new(substituted(right, taken)),
        ),
        ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
            op: *op,
            left: Box::new(substituted(left, taken)),
            right: Box::new(substituted(right, taken)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(substituted(left, taken)),
            right: Box::new(substituted(right, taken)),
        },
        ExprKind::Call {
            function,
            arguments,
            span,
        } => ExprKind::Call {
            function: *function,
            arguments: arguments
                .iter()
                .map(|argument| substituted(argument, taken))
                .collect(),
            span: *span,
        },
        ExprKind::Array(items) => {
            ExprKind::Array(items.iter().map(|item| substituted(item, taken)).collect())
        }
        ExprKind::Set(items) => {
            ExprKind::Set(items.iter().map(|item| substituted(item, taken)).collect())
        }
        // Nothing else can hold a fold: an object's values, a range's ends and a
        // nested read are all refused a fold by the grouping rule, so leaving
        // them as written is what they are.
        other => other.clone(),
    };
    Expr {
        kind,
        span: expr.span,
    }
}
