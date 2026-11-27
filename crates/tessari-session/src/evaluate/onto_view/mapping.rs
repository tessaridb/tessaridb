//! The expression half of the view rewrite: which conjuncts are
//! group-constant over the view's keys, which read values map onto the view's
//! projected keys and folds, and the rewrites that build the read over the
//! stored rows.

use tessari_ql::{Aggregate, Expr, ExprKind, Select};

/// An `AND` chain as the conjuncts it joins; anything else is one conjunct.
pub(super) fn conjuncts_of(condition: &Expr) -> Vec<&Expr> {
    match &condition.kind {
        ExprKind::And(left, right) => {
            let mut held = conjuncts_of(left);
            held.extend(conjuncts_of(right));
            held
        }
        _ => vec![condition],
    }
}

/// Whether nothing in the expression reads beyond the record it is asked
/// about and the statement it was written in: no parameter, subquery, key
/// read or TTL. The same walk the materialized shape makes, for the same
/// reason — the stored rows can answer nothing else.
pub(super) fn answerable(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Not(inner) | ExprKind::Negate(inner) => answerable(inner),
        ExprKind::Route { value, .. } => answerable(value),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            answerable(condition) && answerable(then) && otherwise.as_deref().is_none_or(answerable)
        }
        ExprKind::Coalesce(left, right)
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => answerable(left) && answerable(right),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().all(answerable),
        ExprKind::Object(fields) => fields.iter().all(|field| answerable(&field.value)),
        ExprKind::Range(range) => answerable(&range.start) && answerable(&range.end),
        ExprKind::Call { arguments, .. } => arguments.iter().all(answerable),
        ExprKind::Fold {
            over, at, ahead, ..
        } => {
            over.as_deref().is_none_or(answerable)
                && at.as_deref().is_none_or(answerable)
                && ahead.as_deref().is_none_or(answerable)
        }
        ExprKind::Select(_) | ExprKind::Get(_) | ExprKind::Ttl(_) | ExprKind::Parameter(_) => false,
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Table(_) | ExprKind::Record(_) => true,
    }
}

/// `expr` with every field path replaced by the view's projected name for the
/// group key it names, when every path in it names one — which is what makes
/// the expression group-constant, so evaluating it over a view row keeps or
/// drops the whole group exactly as the base read's condition would.
pub(super) fn over_the_keys(expr: &Expr, view: &Select) -> Option<Expr> {
    let kind = match &expr.kind {
        ExprKind::Path(path) => {
            let name = view.group.iter().find_map(|key| match &key.kind {
                ExprKind::Path(key_path) if key_path.path == path.path => projected_name(key, view),
                _ => None,
            })?;
            ExprKind::Path(tessari_ql::FieldPath {
                path: tessari_types::Path::field(&name),
                span: expr.span,
            })
        }
        ExprKind::Not(inner) => ExprKind::Not(Box::new(over_the_keys(inner, view)?)),
        ExprKind::Negate(inner) => ExprKind::Negate(Box::new(over_the_keys(inner, view)?)),
        ExprKind::And(left, right) => ExprKind::And(
            Box::new(over_the_keys(left, view)?),
            Box::new(over_the_keys(right, view)?),
        ),
        ExprKind::Or(left, right) => ExprKind::Or(
            Box::new(over_the_keys(left, view)?),
            Box::new(over_the_keys(right, view)?),
        ),
        ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
            op: *op,
            left: Box::new(over_the_keys(left, view)?),
            right: Box::new(over_the_keys(right, view)?),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(over_the_keys(left, view)?),
            right: Box::new(over_the_keys(right, view)?),
        },
        ExprKind::Coalesce(left, right) => ExprKind::Coalesce(
            Box::new(over_the_keys(left, view)?),
            Box::new(over_the_keys(right, view)?),
        ),
        ExprKind::Call {
            function,
            arguments,
            span,
        } => ExprKind::Call {
            function: *function,
            arguments: arguments
                .iter()
                .map(|argument| over_the_keys(argument, view))
                .collect::<Option<Vec<_>>>()?,
            span: *span,
        },
        // A literal stands as written; anything else — a fold, a route, a
        // container, a test — is more than a conjunct over the keys needs to
        // be, and declining it keeps the gate simple rather than clever.
        ExprKind::Literal(value) => ExprKind::Literal(value.clone()),
        _ => return None,
    };
    Some(Expr {
        kind,
        span: expr.span,
    })
}

/// The name the view projects `key` under, when it projects it at all.
fn projected_name(key: &Expr, view: &Select) -> Option<String> {
    view.projection
        .written()
        .iter()
        .find(|one| one.value.same_shape(key))
        .map(|one| one.name.text.clone())
}

/// A read group key or projected value that names one of the view's group
/// keys, rewritten to the view's field for it.
pub(super) fn projected_key(expr: &Expr, view: &Select) -> Option<Expr> {
    let name = view
        .group
        .iter()
        .find(|key| key.same_shape(expr))
        .and_then(|key| projected_name(key, view))?;
    Some(Expr {
        kind: ExprKind::Path(tessari_ql::FieldPath {
            path: tessari_types::Path::field(&name),
            span: expr.span,
        }),
        span: expr.span,
    })
}

/// One read value rewritten over the view's rows: a group key becomes the
/// view's field for it, and one of the four distributive folds becomes the
/// same fold over the view's stored fold — with `count` re-folded as the
/// **sum** of the stored counts, which is what a count of the whole is.
pub(super) fn refolded(expr: &Expr, view: &Select) -> Option<Expr> {
    if let ExprKind::Fold {
        fold,
        over,
        at: None,
        ahead: None,
        ..
    } = &expr.kind
        && matches!(
            fold,
            Aggregate::Sum | Aggregate::Count | Aggregate::Min | Aggregate::Max
        )
    {
        let held = view.projection.written().iter().find(|one| {
            matches!(
                &one.value.kind,
                ExprKind::Fold {
                    fold: held_fold,
                    over: held_over,
                    at: None,
                    ahead: None,
                    ..
                } if held_fold == fold && same_over(over, held_over)
            )
        })?;
        let name = Expr {
            kind: ExprKind::Path(tessari_ql::FieldPath {
                path: tessari_types::Path::field(&held.name.text),
                span: expr.span,
            }),
            span: expr.span,
        };
        let refold = match fold {
            Aggregate::Count => Aggregate::Sum,
            held => *held,
        };
        return Some(Expr {
            kind: ExprKind::Fold {
                fold: refold,
                over: Some(Box::new(name)),
                at: None,
                ahead: None,
                distinct: false,
                filter: None,
                span: expr.span,
            },
            span: expr.span,
        });
    }
    projected_key(expr, view)
}

/// Two folds' `over` agreeing: both absent, or the same expression.
fn same_over(left: &Option<Box<Expr>>, right: &Option<Box<Expr>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(one), Some(two)) => one.same_shape(two),
        _ => false,
    }
}
