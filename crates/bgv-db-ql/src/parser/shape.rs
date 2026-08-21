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

use bgv_db_types::Number;

use super::Parser;
use crate::ast::{ExprKind, FieldPath, Ordering, Projectable, Projection};
use crate::error::{Error, Result};
use crate::token::{Punct, Span, Token};

impl Parser<'_> {
    /// `GROUP BY city, address.country`, when it is there.
    pub(super) fn group_by(&mut self) -> Result<Vec<FieldPath>> {
        if !self.eat_word("group") {
            return Ok(Vec::new());
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `GROUP`"));
        }
        let mut keys = vec![self.field_path()?];
        while self.eat_punct(Punct::Comma) {
            keys.push(self.field_path()?);
        }
        Ok(keys)
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
pub(super) fn check_grouping(projection: &Projection, group: &[FieldPath]) -> Result<()> {
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
    let folds = values
        .iter()
        .any(|value| matches!(value.value, Projectable::Aggregate { .. }));
    if !folds && group.is_empty() {
        return Ok(());
    }
    for value in values {
        let Projectable::Value(expr) = &value.value else {
            continue;
        };
        let ExprKind::Path(path) = &expr.kind else {
            return Err(Error::UngroupedProjection {
                name: value.name.text.clone(),
                span: expr.span,
            });
        };
        if !group.iter().any(|key| key.path == path.path) {
            return Err(Error::UngroupedProjection {
                name: value.name.text.clone(),
                span: expr.span,
            });
        }
    }
    Ok(())
}
