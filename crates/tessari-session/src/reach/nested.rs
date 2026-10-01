//! The tables a nested expression, read or source names.

use super::in_join_side;
use tessari_ql::{Expr, ExprKind, Projection, Select, Source, TableRef};

/// The tables an expression reaches.
///
/// An expression can hold a read — `(SELECT …)` is an ordinary term — and a
/// read names tables. Nothing else in an expression does: a path is a route
/// inside a record the caller already reached, and a literal names nothing.
///
/// Exhaustive for the two arms that carry a table and deliberately shallow
/// everywhere else, walked recursively so that a read nested two groups deep is
/// found as surely as one written at the top.
pub(crate) fn in_expr(expr: &Expr) -> Vec<&TableRef> {
    match &expr.kind {
        ExprKind::Select(select) => in_select(select),
        // A point read of one record names that record's table.
        ExprKind::Record(target) | ExprKind::Get(target) | ExprKind::Ttl(target) => {
            vec![&target.table]
        }
        ExprKind::Table(table) => vec![table],
        ExprKind::Not(inner) | ExprKind::Negate(inner) => in_expr(inner),
        // Both arms of a conditional, because either may run and a permission
        // question is asked before anything does.
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            let mut found = in_expr(condition);
            found.extend(in_expr(then));
            if let Some(otherwise) = otherwise {
                found.extend(in_expr(otherwise));
            }
            found
        }
        // The right side of a coalesce runs only when the left holds nothing —
        // and it is still named here, because whether it runs is a property of
        // the data and a grant must not depend on one.
        ExprKind::Coalesce(left, right) => {
            let mut found = in_expr(left);
            found.extend(in_expr(right));
            found
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            let mut found = in_expr(left);
            found.extend(in_expr(right));
            found
        }
        ExprKind::Fold { over, .. } => over.as_deref().map(in_expr).unwrap_or_default(),
        ExprKind::Call { arguments, .. } => arguments.iter().flat_map(in_expr).collect(),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().flat_map(in_expr).collect(),
        ExprKind::Object(fields) => fields
            .iter()
            .flat_map(|field| in_expr(&field.value))
            .collect(),
        ExprKind::Range(range) => {
            let mut found = in_expr(&range.start);
            found.extend(in_expr(&range.end));
            found
        }
        // A literal, a parameter and a path name nothing.
        ExprKind::Literal(_) | ExprKind::Parameter(_) | ExprKind::Path(_) => Vec::new(),
    }
}

/// Every table a read reaches — its source **and** its expressions.
///
/// # The half that was missing, and what it cost
///
/// This used to answer with the source alone, and that was a grant bypass
/// rather than an omission. A projection, a `WHERE`, an `ORDER BY` and a
/// `GROUP BY` are all expressions, and an expression may hold a read: `SELECT
/// (SELECT pay FROM salaries) AS leaked FROM public` names `public` as its
/// source and answers with `salaries`. A caller granted `read` on `public`
/// alone was handed the contents of a table nobody granted them, with no error
/// anywhere, because the loop that checks grants was given a list the second
/// table was never on.
///
/// The rule that replaces it is the one the module header already stated for
/// statements: **every table the statement can reach is named here**, wherever
/// in it the name was written.
pub(crate) fn in_select(select: &Select) -> Vec<&TableRef> {
    let mut found = in_source(&select.from);
    if let Projection::Values {
        values: projected, ..
    } = &select.projection
    {
        for one in projected {
            found.extend(in_expr(&one.value));
        }
    }
    for key in &select.group {
        found.extend(in_expr(key));
    }
    for key in &select.order {
        found.extend(in_expr(&key.key));
    }
    found
}

/// The tables a read's source names.
pub(crate) fn in_source(from: &Source) -> Vec<&TableRef> {
    match from {
        // **No table, and that emptiness is the `BACKUP` shape** — a loop
        // reading "every table it names is granted" passes over an empty list
        // for a reason that has nothing to do with permission. So this one is
        // not governed here at all: `Needs::of` classifies it `Administer`
        // before this list is consulted, and `within_grants` refuses a
        // grant-governed user by role rather than by an empty answer.
        Source::Node => Vec::new(),
        // A search names no table: each member is read under its own table's
        // grants when the search runs (ADR-0105), so a member nobody granted is
        // left out rather than refused. Only its condition can hold a read.
        Source::Search { condition, .. } => condition.as_deref().map(in_expr).unwrap_or_default(),
        Source::Record(target) => vec![&target.table],
        Source::Table(table) | Source::Range { table, .. } => vec![table],
        // The condition is an expression, and an expression may hold a read.
        Source::Where { table, condition } => {
            let mut found = vec![table];
            found.extend(in_expr(condition));
            found
        }
        // **Every** table in the chain counts, not only the first and the last.
        // A traversal that could read records in a table nobody granted, because
        // the edge table was granted, is a way around the grant rather than a
        // use of it — and a walk of several hops passes through several tables,
        // each of which somebody has to have been granted.
        Source::Traverse { from, hops, .. } => {
            let mut found = vec![&from.table];
            for hop in hops {
                found.push(&hop.edges);
                found.extend(hop.target.as_ref());
            }
            found
        }
        // A side may be a read rather than a table, and a read reaches
        // everything `in_select` reaches — its own source, its projection, its
        // grouping and its ordering. A join side that only contributed a table
        // name would be a way around the grant for every table its inner read
        // could name.
        Source::Join {
            left,
            right,
            condition,
            ..
        } => {
            let mut found = in_join_side(left);
            found.extend(in_join_side(right));
            if let Some(condition) = condition {
                found.extend(in_expr(condition));
            }
            found
        }
        // The condition is an expression, and an expression may hold a read —
        // the same reason `Source::Where` walks its own.
        Source::Subquery { read, condition } => {
            let mut found = in_select(read);
            if let Some(condition) = condition {
                found.extend(in_expr(condition));
            }
            found
        }
    }
}
