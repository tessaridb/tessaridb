//! Checks a parsed read must pass: cursors, grouping, folds and where several values may stand.

use super::holds_a_fold;
use crate::ast::{Expr, ExprKind, Ordering, Projection, RecordTarget, Source};
use crate::error::{Error, Result};
use crate::token::Span;

/// What a cursor may be written beside, and which table its anchor may name.
///
/// Both refusals are properties of the statement, so neither waits for a read.
///
/// **A `START` beside an `AFTER`** is refused because the two are answers to the
/// same question — where does this page begin — and applying both means one of
/// them silently loses: the offset would count from the cursor's own position
/// and skip a page nobody asked to skip.
///
/// **An anchor from another table** is refused because a record identity carries
/// no table once it is compared. `orders:5` and `users:5` compare identically,
/// so a cursor pasted from the wrong page would page a real table by a real
/// identity and answer with records — the wrong ones, quietly. The check is
/// possible only where the source names one table; a join, a walk and a
/// materialised source each reach records from more than one place, and there is
/// no name there to disagree with.
///
/// **A clause that changes what a row is** is refused beside it for a third
/// reason: an anchor is a record, and `GROUP BY` answers with groups, `FETCH`
/// answers with records whose references have been opened, and `SPLIT ON`
/// answers with a row per element. In each of those the thing the cursor is
/// compared against is not the thing the anchor is, so the comparison would be
/// between two different kinds of row and the page would be decided by whichever
/// of them the sort key happened to reach.
pub(crate) fn check_cursor(
    from: &Source,
    after: Option<&RecordTarget>,
    start: Option<u64>,
    reshaping: [(&'static str, bool); 3],
) -> Result<()> {
    let Some(anchor) = after else {
        return Ok(());
    };
    if start.is_some() {
        return Err(Error::CursorBesideAnOffset { span: anchor.span });
    }
    for (clause, written) in reshaping {
        if written {
            return Err(Error::CursorBesideAReshaping {
                clause,
                span: anchor.span,
            });
        }
    }
    let named = match from {
        Source::Table(table) | Source::Where { table, .. } | Source::Range { table, .. } => {
            &table.name
        }
        Source::Record(target) => &target.table.name,
        Source::Node | Source::Traverse { .. } | Source::Join { .. } | Source::Subquery { .. } => {
            return Ok(());
        }
    };
    if !anchor.table.name.text.eq_ignore_ascii_case(&named.text) {
        return Err(Error::AnchorFromAnotherTable {
            anchor: anchor.table.name.text.clone(),
            table: named.text.clone(),
            span: anchor.span,
        });
    }
    Ok(())
}

/// A grouped read may project only its keys and its folds.
///
/// `SELECT name, count(*) AS n … GROUP BY city` is refused, because `name` has
/// as many values as the group has records and picking one silently is how a
/// wrong number reaches a report. It is a property of the statement, so it is
/// refused when the statement is read.
pub(crate) fn check_grouping(projection: &Projection, group: &[Expr]) -> Result<()> {
    let Projection::Values { values, .. } = projection else {
        // `SELECT *` over a group would answer with whichever record came last.
        if group.is_empty() {
            return Ok(());
        }
        return Err(Error::UngroupedProjection {
            name: "*".to_owned(),
            span: Span::new(0, 0),
        });
    };
    let folds = values.iter().any(|value| holds_a_fold(&value.value));
    if !folds && group.is_empty() {
        return Ok(());
    }
    for value in values {
        if !grouped_by(&value.value, group) {
            return Err(Error::UngroupedProjection {
                name: value.name.text.clone(),
                span: value.value.span,
            });
        }
        nested_fold(&value.value)?;
    }
    Ok(())
}

/// Whether this expression has one value per group.
///
/// Recursive, because a projection may now be *built from* folds and keys rather
/// than being one: `mean(age) * 2` is admissible and `name` is not, and the
/// difference is a property of every part rather than of the whole.
///
/// - A **fold** has one value per group by definition, and what is inside it is
///   per-record and is not this rule's business.
/// - An expression with the **shape of a group key** has one value per group,
///   because that is what grouping by it means. Compared by shape rather than by
///   `==`, since the same expression written twice sits at two spans and would
///   otherwise never match itself.
/// - A **literal** is one value everywhere.
/// - Anything built out of those is one value per group.
///
/// What is left is a path, a parameter or a read that reaches into the record,
/// and each of those has as many values as the group has records — which is how
/// a wrong number reaches a report.
pub(crate) fn grouped_by(expr: &Expr, group: &[Expr]) -> bool {
    if group.iter().any(|key| key.same_shape(expr)) {
        return true;
    }
    match &expr.kind {
        ExprKind::Fold { .. } | ExprKind::Literal(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => grouped_by(inner, group),
        // Every arm has to be grouped, not just the one that will run: which
        // one runs is a property of the data, and whether a projection is legal
        // is a property of the statement.
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            grouped_by(condition, group)
                && grouped_by(then, group)
                && otherwise
                    .as_deref()
                    .is_none_or(|otherwise| grouped_by(otherwise, group))
        }
        ExprKind::Coalesce(left, right) => grouped_by(left, group) && grouped_by(right, group),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            grouped_by(left, group) && grouped_by(right, group)
        }
        ExprKind::Call { arguments, .. } => {
            arguments.iter().all(|argument| grouped_by(argument, group))
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            items.iter().all(|item| grouped_by(item, group))
        }
        ExprKind::Object(fields) => fields.iter().all(|field| grouped_by(&field.value, group)),
        // A path, a parameter, a table, a record, a range, a read: none of them
        // is one value per group unless it *is* a key, which was asked above.
        _ => false,
    }
}

/// A fold inside a fold is refused, and refused where the statement is read.
///
/// `mean(sum(price))` has no meaning at one grouping level: the inner fold has
/// already collapsed the records the outer one would fold over, so what is left
/// to average is a single number. It is a property of the statement, so nothing
/// has to run for it to be wrong.
pub(crate) fn nested_fold(expr: &Expr) -> Result<()> {
    if let ExprKind::Fold {
        over: Some(over),
        span,
        ..
    } = &expr.kind
        && holds_a_fold(over)
    {
        return Err(Error::FoldInsideAFold { span: *span });
    }
    for child in children(expr) {
        nested_fold(child)?;
    }
    Ok(())
}

/// The expressions one expression is built out of.
pub(crate) fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Fold { over, .. } => over.as_deref().into_iter().collect(),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => vec![inner],
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            let mut parts = vec![&**condition, &**then];
            parts.extend(otherwise.as_deref());
            parts
        }
        ExprKind::Coalesce(left, right) => vec![left, right],
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

/// A fold stands in a projection and nowhere else.
///
/// A filter sees one record at a time, so a fold in a `WHERE` is asking a
/// question the filter cannot be handed the records to answer — and what it
/// *means* is a filter over groups, which is `HAVING`: a second filter position
/// with its own scoping rule, and its own row in the specification's list of
/// absences. The refusal says which of the two it is, because "unexpected token"
/// would send the author looking for a typo.
///
/// An `ORDER BY` and a `GROUP BY` key are refused for the same reason: both are
/// evaluated per record, before there is a group to fold over.
pub(crate) fn check_fold_positions(
    from: &Source,
    group: &[Expr],
    order: &[Ordering],
) -> Result<()> {
    match from {
        Source::Where { condition, .. } => no_fold(condition)?,
        Source::Join {
            condition: Some(condition),
            ..
        }
        | Source::Subquery {
            condition: Some(condition),
            ..
        } => no_fold(condition)?,
        // The inner read was checked as it was parsed, so there is nothing left
        // to say about it here.
        Source::Node
        | Source::Record(_)
        | Source::Table(_)
        | Source::Range { .. }
        | Source::Traverse { .. }
        | Source::Join { .. }
        | Source::Subquery { .. } => {}
    }
    for key in group {
        no_fold(key)?;
    }
    for ordering in order {
        no_fold(&ordering.key)?;
    }
    Ok(())
}

/// Refuse a fold anywhere in this expression.
pub(crate) fn no_fold(expr: &Expr) -> Result<()> {
    if let ExprKind::Fold { span, .. } = &expr.kind {
        return Err(Error::FoldInAFilter { span: *span });
    }
    for child in children(expr) {
        no_fold(child)?;
    }
    Ok(())
}

/// A route reaching several values stands as the left operand of a comparison,
/// and nowhere else yet.
///
/// The right operand is excluded too: `'urgent' = tags[*]` would be the same
/// question written backwards, and giving it a second spelling before the first
/// one has a projection and an index is how a language grows two ways to ask
/// one thing.
pub(crate) fn check_several(expr: &Expr) -> Result<()> {
    if let ExprKind::Binary { left, right, .. } = &expr.kind {
        // The one admitted position. What is under it still has to be checked —
        // `a[*].b[*]` is two relations composed, and composing them is its own
        // question.
        if let ExprKind::Path(field) = &left.kind
            && field.path.is_several()
        {
            return check_several(right);
        }
    }
    no_several(expr)
}

/// A route reaching several values stands as the **whole** projected value, and
/// nowhere inside a larger one.
///
/// A projection collects, so `tags[*] AS all_tags` answers with every value the
/// route reaches. `array::len(tags[*])` is refused because it has two defensible
/// answers — the function over the collected values, or the function applied to
/// each of them — and a language that picks one silently teaches the other by
/// surprise.
pub(crate) fn check_projected(expr: &Expr) -> Result<()> {
    if let ExprKind::Path(field) = &expr.kind
        && field.path.is_several()
    {
        return Ok(());
    }
    no_several(expr)
}

/// Refuse a route reaching several values anywhere in this expression.
pub(crate) fn no_several(expr: &Expr) -> Result<()> {
    if let ExprKind::Path(field) = &expr.kind
        && field.path.is_several()
    {
        return Err(Error::SeveralOutsideAComparison { span: field.span });
    }
    for child in children(expr) {
        // A comparison nested inside something else — `NOT tags[*] = 'x'`, or
        // one side of an `AND` — is still a comparison, so it keeps its rule.
        check_several(child)?;
    }
    Ok(())
}
