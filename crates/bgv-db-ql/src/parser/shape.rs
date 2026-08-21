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
use crate::ast::{Expr, FieldPath, Ordering, Projectable, Projection};
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
        // The projection has to *be* a group key. Compared by shape rather than
        // by `==`, because the same expression written twice in one statement
        // sits at two spans and would otherwise never match itself.
        if !group.iter().any(|key| key.same_shape(expr)) {
            return Err(Error::UngroupedProjection {
                name: value.name.text.clone(),
                span: expr.span,
            });
        }
    }
    Ok(())
}
