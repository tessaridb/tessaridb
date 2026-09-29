//! Inserts, answers and the edit an UPDATE or UPSERT carries.

use super::super::Parser;
use crate::ast::{Answer, Assignment, Edit, Expr, RecordTarget, StatementKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct};

impl Parser<'_> {
    /// `INSERT INTO users (name, email) VALUES ('ada', 'a@x'), ('grace', 'g@x')`
    ///
    /// # Why `INTO` and `VALUES` are not reserved words
    ///
    /// They are matched as plain words, the way `BEFORE` and `AFTER` are.
    /// Reserving them would be a cost paid by every script that has a field
    /// called `values`, for a benefit nobody collects: both appear in exactly
    /// one position in exactly one statement, and neither is ambiguous there.
    ///
    /// # Why the arity is checked here
    ///
    /// A row of the wrong length is a mistyped statement, and the alternative is
    /// finding out at the write with part of the batch already decided — which
    /// makes a typing mistake arrive wearing the shape of a write failure.
    pub(crate) fn insert_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        if !self.eat_word("into") {
            return Err(self.error_here("`INTO` and the table to write to"));
        }
        let table = self.table_ref()?;

        // Named fields, not values: a caller's text cannot arrive in this
        // position and be read as a field name, which is the same property the
        // query builder is built around.
        self.expect_punct(Punct::ParenOpen, "`(` and the fields each row supplies")?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.name()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::ParenClose, "`)` after the field list")?;

        if !self.eat_word("values") {
            return Err(self.error_here("`VALUES` and at least one row"));
        }

        let mut rows: Vec<Vec<Expr>> = Vec::new();
        loop {
            let opened = self.span_here();
            self.expect_punct(Punct::ParenOpen, "`(` and a row of values")?;
            let mut row = Vec::new();
            loop {
                row.push(self.expression()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::ParenClose, "`)` after the row's values")?;

            if row.len() != columns.len() {
                return Err(Error::InsertRowArity {
                    // Counted from one, because the author is counting rows on
                    // the screen and not indexing an array.
                    row: rows.len().saturating_add(1),
                    found: row.len(),
                    expected: columns.len(),
                    span: opened,
                });
            }
            rows.push(row);

            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }

        Ok(StatementKind::Insert {
            table,
            columns,
            rows,
        })
    }

    /// `RETURN BEFORE` or `RETURN AFTER`, when the write carries one.
    ///
    /// Refused where it could only ever answer `NONE`: there is no record before
    /// a `CREATE` and none after a `DELETE`. Answering `NONE` to a question the
    /// author plainly meant is the silent-wrong-answer shape this language
    /// spends its rules removing, so the refusal names the two words that work.
    pub(crate) fn answer(&mut self, verb: Keyword) -> Result<Answer> {
        if !self.eat_keyword(Keyword::Return) {
            return Ok(Answer::Nothing);
        }
        let before = self.eat_word("before");
        if !before && !self.eat_word("after") {
            return Err(self.error_here("`BEFORE` or `AFTER` after `RETURN`"));
        }
        match (verb, before) {
            (Keyword::Create, true) => Err(self.error_here(
                "`AFTER` — a create has no record before it, so `BEFORE` could only answer NONE",
            )),
            (Keyword::Delete, false) => Err(self.error_here(
                "`BEFORE` — a delete has no record after it, so `AFTER` could only answer NONE",
            )),
            (_, true) => Ok(Answer::Before),
            (_, false) => Ok(Answer::After),
        }
    }

    /// The statement a change verb makes of a target, an edit and an answer.
    pub(crate) fn changed(
        verb: Keyword,
        target: RecordTarget,
        edit: Edit,
        condition: Option<Expr>,
        answer: Answer,
    ) -> StatementKind {
        if verb == Keyword::Upsert {
            StatementKind::Upsert {
                target,
                edit,
                answer,
            }
        } else {
            StatementKind::Update {
                target,
                edit,
                condition,
                answer,
            }
        }
    }

    /// `WHERE <condition>` after an edit — the compare-and-set clause.
    ///
    /// `UPDATE` only. `UPSERT` asserts nothing about the record it writes, so a
    /// condition on it has no meaning to give; it is refused here rather than
    /// parsed and ignored, because a clause that parses and does nothing is the
    /// shape a caller trusts.
    pub(crate) fn edit_condition(&mut self, verb: Keyword) -> Result<Option<Expr>> {
        if self.peek_keyword() != Some(Keyword::Where) {
            return Ok(None);
        }
        if verb == Keyword::Upsert {
            return Err(self.error_here(
                "no `WHERE` — `UPSERT` writes the record whether or not it is                  there, so there is no prior state to test; use `UPDATE` to                  change a record only when it already says something",
            ));
        }
        self.advance();
        // `condition()` and not `expression()`, and the difference is the whole
        // clause: in a **condition** position a bare name is a route into the
        // record, and in a value position it is a table. Parsed as an
        // expression, `WHERE visits = 3` asks for a table called `visits`.
        let condition = self.condition()?;
        crate::parser::shape::no_fold(&condition)?;
        crate::parser::shape::check_several(&condition)?;
        Ok(Some(condition))
    }

    /// `name = 'grace'` — one route and what it becomes.
    pub(crate) fn assignment(&mut self) -> Result<Assignment> {
        let route = self.field_path()?;
        // A route reaching several values would have to say which of them
        // changes, and `[*]`'s three contexts do not include this one.
        crate::parser::shape::no_several_path(&route)?;
        self.expect_punct(Punct::Equals, "`=` and what the field becomes")?;
        // The **condition** position, so a bare name is a route into the record
        // rather than a table — the reading a `WHERE`, an `ORDER BY` and a
        // projection all give it. `SET visits = visits + 1` is the whole point,
        // and in the value position `visits` would be a table.
        Ok(Assignment {
            route,
            value: self.condition()?,
        })
    }
}
