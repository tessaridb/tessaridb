//! What an expression reads.
//!
//! Two walks over one tree, kept in one module deliberately: the coarse
//! question — does this touch the record at all — decides whether a filter's
//! right-hand side can be an index bound and whether the constant fold may
//! evaluate it once; the fine question — which of the record's fields — decides
//! what an ordering depends on. Two walks that drifted apart would each still
//! compile, and the disagreement would surface as a read that silently loses
//! its ordering.

use std::collections::BTreeSet;

use tessari_ql::{Expr, ExprKind};

/// Whether an expression reads the record being tested.
pub(super) fn reads_a_record(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Path(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => reads_a_record(inner),
        ExprKind::And(left, right) | ExprKind::Or(left, right) => {
            reads_a_record(left) || reads_a_record(right)
        }
        ExprKind::Arithmetic { left, right, .. } | ExprKind::Binary { left, right, .. } => {
            reads_a_record(left) || reads_a_record(right)
        }
        ExprKind::Call { arguments, .. } => arguments.iter().any(reads_a_record),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(reads_a_record),
        ExprKind::Object(fields) => fields.iter().any(|field| reads_a_record(&field.value)),
        ExprKind::Range(range) => reads_a_record(&range.start) || reads_a_record(&range.end),
        // A fold reads records, so it is not constant and must never be
        // evaluated once above the loop.
        //
        // Today nothing asks: a projection holding a fold is answered by the
        // grouped path, which never reaches the constant-folding pass. Answering
        // `false` here would still be a statement about the world that is
        // wrong — this question is "can this be computed without records", and
        // for a fold it cannot — and the day the two paths are reordered, a
        // wrong `false` becomes a fold evaluated with no group to fold over.
        ExprKind::Fold { .. } => true,
        // A parameter is a value, so it reads no record — and after binding
        // there is none left to ask.
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Select(_) => false,
    }
}

/// Every top-level field name an expression reads, however deeply nested.
///
/// The neighbour of [`reads_a_record`], answering the finer question: not
/// *whether* the tree touches the record but **which** of its fields. Kept
/// beside it deliberately — two walks over the same tree that drifted apart
/// would each still compile, and the disagreement would surface as a read that
/// silently loses its ordering.
///
/// Exhaustive over `ExprKind` with no wildcard, so a new expression kind is a
/// compile error here rather than a node this walk quietly steps over.
pub(crate) fn roots_read(expr: &Expr, into: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Path(field) => {
            into.insert(field.path.root().to_owned());
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => roots_read(inner, into),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            roots_read(left, into);
            roots_read(right, into);
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                roots_read(argument, into);
            }
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                roots_read(item, into);
            }
        }
        ExprKind::Object(fields) => {
            for field in fields {
                roots_read(&field.value, into);
            }
        }
        ExprKind::Range(range) => {
            roots_read(&range.start, into);
            roots_read(&range.end, into);
        }
        ExprKind::Fold { over, .. } => {
            if let Some(over) = over {
                roots_read(over, into);
            }
        }
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Select(_) => {}
    }
}
