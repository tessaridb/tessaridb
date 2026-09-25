//! What an expression reads, and what it will not answer twice alike.
//!
//! Three walks over one tree, kept in one module deliberately: the coarse
//! question — does this touch the record at all — decides whether a filter's
//! right-hand side can be an index bound; the fine question — which of the
//! record's fields — decides what an ordering depends on; and the third asks
//! whether anything in the tree gives a fresh answer every time it is asked.
//! Walks that drifted apart would each still compile, and the disagreement
//! would surface as a read that silently loses its ordering.
//!
//! The first and the third are the constant fold's condition **together**, and
//! that is the correction this module carries. Reading no record is not the same
//! property as being safe to evaluate once: `rand::uuid()` reads none and must
//! still be asked again for every record, and the fold that asked only the first
//! question handed every row of one read the same identifier.

use std::collections::BTreeSet;

use tessari_ql::{Expr, ExprKind, Purity};

/// Whether an expression reads the record being tested.
pub(super) fn reads_a_record(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Path(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => reads_a_record(inner),
        // Every arm, not only the one that will run. Which arm runs is a
        // property of the record, so an expression whose *untaken* arm reads
        // one is still not constant.
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            reads_a_record(condition)
                || reads_a_record(then)
                || otherwise.as_deref().is_some_and(reads_a_record)
        }
        ExprKind::Coalesce(left, right) => reads_a_record(left) || reads_a_record(right),
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
        | ExprKind::Ttl(_)
        | ExprKind::Select(_) => false,
    }
}

/// Whether any part of an expression answers afresh every time it is asked.
///
/// The half of the fold's condition that [`reads_a_record`] cannot supply. It is
/// asked of the whole tree rather than of the outermost node, because a
/// generator nested anywhere makes the tree above it unfoldable too:
/// `type::string(rand::uuid())` reads no record at either level, and folded at
/// the top it is one identifier written into every row just as surely.
///
/// Exhaustive over `ExprKind` with no wildcard, for the reason [`roots_read`]
/// gives — a new expression kind must be a compile error here and not a node
/// this walk steps over into a wrong `false`.
pub(super) fn answers_afresh(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Call {
            function,
            arguments,
            ..
        } => function.purity() == Purity::PerCall || arguments.iter().any(answers_afresh),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => answers_afresh(inner),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            answers_afresh(condition)
                || answers_afresh(then)
                || otherwise.as_deref().is_some_and(answers_afresh)
        }
        ExprKind::Coalesce(left, right)
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => answers_afresh(left) || answers_afresh(right),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(answers_afresh),
        ExprKind::Object(fields) => fields.iter().any(|field| answers_afresh(&field.value)),
        ExprKind::Range(range) => answers_afresh(&range.start) || answers_afresh(&range.end),
        ExprKind::Fold { over, .. } => over.as_deref().is_some_and(answers_afresh),
        // A subquery is **not** descended into, and that is a decision rather
        // than an omission. It is its own read with its own fold, so a generator
        // inside it is already asked afresh for each of *its* records; whether
        // the subquery as a whole should re-run for every outer record is the
        // pre-existing question of subquery hoisting — every subquery
        // independent of the outer record is evaluated once today — and
        // answering it here would change far more than this wave asked (Q-226).
        ExprKind::Literal(_)
        | ExprKind::Path(_)
        | ExprKind::Parameter(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Ttl(_)
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
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            roots_read(condition, into);
            roots_read(then, into);
            if let Some(otherwise) = otherwise {
                roots_read(otherwise, into);
            }
        }
        ExprKind::Coalesce(left, right) => {
            roots_read(left, into);
            roots_read(right, into);
        }
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
        | ExprKind::Ttl(_)
        | ExprKind::Select(_) => {}
    }
}
