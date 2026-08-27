//! The clauses that shape a read rather than choose what it reads.
//!
//! `GROUP BY`, `ORDER BY`, `START` and `LIMIT`, and the one rule that binds
//! them: a grouped read may answer only with its keys and its folds.
//!
//! The words are **contextual**, not reserved. Nothing else can stand in the
//! positions they appear in, so nothing is ambiguous — and reserving `order`,
//! `by`, `group`, `limit`, `start`, `asc` or `desc` would take seven perfectly
//! good names away from data that already exists. This language has a rule
//! about that.

use tessari_types::Number;

use super::Parser;
use crate::ast::{Expr, ExprKind, FieldPath, Ordering, Projection, Source};
use crate::error::{Error, Result};
use crate::token::{Punct, Span, Token};

impl Parser<'_> {
    /// `GROUP BY city, address.country`, when it is there.
    pub(super) fn group_by(&mut self) -> Result<Vec<Expr>> {
        if !self.eat_word("group") {
            return Ok(Vec::new());
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `GROUP`"));
        }
        // Read in the condition position, so a bare name is a route into the
        // record — the reading `WHERE` and `ORDER BY` already give it — and an
        // expression works, which is what makes a window sayable.
        let mut keys = vec![self.condition()?];
        while self.eat_punct(Punct::Comma) {
            keys.push(self.condition()?);
        }
        Ok(keys)
    }

    /// `FETCH author, meta.editor`, when it is there.
    ///
    /// `fetch` is a **contextual** word and not a reserved one, the same
    /// decision `ORDER`, `GROUP`, `START` and `LIMIT` took: a field called
    /// `fetch` keeps working, and a language that takes a common noun away from
    /// its users to buy a clause has made a poor trade.
    pub(super) fn fetch_paths(&mut self) -> Result<Vec<FieldPath>> {
        if !self.eat_word("fetch") {
            return Ok(Vec::new());
        }
        let mut routes = vec![self.field_path()?];
        while self.eat_punct(Punct::Comma) {
            routes.push(self.field_path()?);
        }
        Ok(routes)
    }

    /// `ORDER BY name, address.city DESC`, when it is there.
    ///
    /// Keys are read in the condition position, so a bare name is a route into
    /// the record — the same reading a `WHERE` gives it, and the same one a
    /// projection gives it.
    pub(super) fn order_by(&mut self) -> Result<Vec<Ordering>> {
        if !self.eat_word("order") {
            return Ok(Vec::new());
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `ORDER`"));
        }
        let mut keys = vec![self.ordering()?];
        while self.eat_punct(Punct::Comma) {
            keys.push(self.ordering()?);
        }
        Ok(keys)
    }

    fn ordering(&mut self) -> Result<Ordering> {
        let key = self.condition()?;
        // `ASC` is accepted and means nothing, because a reader who writes it is
        // saying what they mean and a grammar that refused would be pedantry.
        let descending = if self.eat_word("desc") {
            true
        } else {
            self.eat_word("asc");
            false
        };
        Ok(Ordering { key, descending })
    }

    /// `LIMIT 10` or `START 20`, when it is there.
    pub(super) fn bound(&mut self, word: &str) -> Result<Option<u64>> {
        if !self.eat_word(word) {
            return Ok(None);
        }
        let expected = "a whole number";
        let Some(Token::Number(Number::Integer(count))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let count = u64::try_from(*count).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(Some(count))
    }
}

/// A grouped read may project only its keys and its folds.
///
/// `SELECT name, count(*) AS n … GROUP BY city` is refused, because `name` has
/// as many values as the group has records and picking one silently is how a
/// wrong number reaches a report. It is a property of the statement, so it is
/// refused when the statement is read.
pub(super) fn check_grouping(projection: &Projection, group: &[Expr]) -> Result<()> {
    let Projection::Values(values) = projection else {
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
fn grouped_by(expr: &Expr, group: &[Expr]) -> bool {
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

/// Whether this expression holds a fold anywhere inside it.
fn holds_a_fold(expr: &Expr) -> bool {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        return true;
    }
    children(expr).into_iter().any(holds_a_fold)
}

/// A fold inside a fold is refused, and refused where the statement is read.
///
/// `mean(sum(price))` has no meaning at one grouping level: the inner fold has
/// already collapsed the records the outer one would fold over, so what is left
/// to average is a single number. It is a property of the statement, so nothing
/// has to run for it to be wrong.
fn nested_fold(expr: &Expr) -> Result<()> {
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
fn children(expr: &Expr) -> Vec<&Expr> {
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
pub(super) fn check_fold_positions(
    from: &Source,
    group: &[Expr],
    order: &[Ordering],
) -> Result<()> {
    match from {
        Source::Where { condition, .. } => no_fold(condition)?,
        Source::Join {
            condition: Some(condition),
            ..
        } => no_fold(condition)?,
        Source::Node
        | Source::Record(_)
        | Source::Table(_)
        | Source::Traverse { .. }
        | Source::Join { .. } => {}
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
pub(super) fn no_fold(expr: &Expr) -> Result<()> {
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
pub(super) fn check_several(expr: &Expr) -> Result<()> {
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
pub(super) fn check_projected(expr: &Expr) -> Result<()> {
    if let ExprKind::Path(field) = &expr.kind
        && field.path.is_several()
    {
        return Ok(());
    }
    no_several(expr)
}

/// Refuse a route reaching several values anywhere in this expression.
pub(super) fn no_several(expr: &Expr) -> Result<()> {
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

/// Refuse a route reaching several values where a bare route is written.
///
/// An index's fields, a `FETCH` route, a join key: each would need the rule its
/// own task will give it.
pub(super) fn no_several_path(field: &FieldPath) -> Result<()> {
    if field.path.is_several() {
        return Err(Error::SeveralOutsideAComparison { span: field.span });
    }
    Ok(())
}
