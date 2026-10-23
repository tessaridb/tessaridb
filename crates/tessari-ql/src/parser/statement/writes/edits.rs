//! Inserts, answers and the edit an UPDATE or UPSERT carries.

use super::super::Parser;
use crate::ast::{
    Answer, Assignment, Edit, Expr, Projection, RecordTarget, Select, StatementKind, TableRef,
    WriteExpiry,
};
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
        // `PARTITION <n>`: the partition of a split topic the rows land in,
        // for a writer that chose it (ADR-0131).
        let partition = if self.eat_word("partition") {
            Some(self.expression()?)
        } else {
            None
        };

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

        let read = if self.peek_keyword() == Some(Keyword::Select) {
            Some(Box::new(self.insert_read(columns.len())?))
        } else if self.eat_word("values") {
            None
        } else {
            return Err(self.error_here("`VALUES` and at least one row, or `SELECT` and a read"));
        };

        let mut rows: Vec<Vec<Expr>> = Vec::new();
        while read.is_none() {
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

        let expire = self.write_expiry()?;
        let answer = if self.eat_keyword(Keyword::Return) {
            if !self.eat_word("position") {
                return Err(self.error_here(
                    "`POSITION` — an insert answers its identities, or with `RETURN POSITION` the positions its messages took",
                ));
            }
            Answer::Position
        } else {
            Answer::Nothing
        };
        Ok(StatementKind::Insert {
            table,
            columns,
            rows,
            read,
            expire,
            answer,
            partition,
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
        if self.peek_word("position") {
            if verb != Keyword::Create {
                return Err(self.error_here(
                    "`BEFORE` or `AFTER` — only a create or an insert takes a position, as a message appended to a topic",
                ));
            }
            self.eat_word("position");
            return Ok(Answer::Position);
        }
        let before = self.eat_word("before");
        if !before && !self.eat_word("after") {
            return Err(self.error_here("`BEFORE`, `AFTER` or `POSITION` after `RETURN`"));
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
        expire: Option<WriteExpiry>,
    ) -> StatementKind {
        if verb == Keyword::Upsert {
            StatementKind::Upsert {
                target,
                edit,
                answer,
                expire,
            }
        } else {
            StatementKind::Update {
                target,
                edit,
                condition,
                answer,
                expire,
            }
        }
    }

    /// `UPDATE readings SET checked = true WHERE stale = true [LIMIT n]` — the
    /// set form, read from just after the table (ADR-0134 D1).
    ///
    /// `SET` and `MERGE` only: a whole value would replace every matched record
    /// with one object and drop every field it does not name on all of them,
    /// which no set write means. The `WHERE` is required, as a set delete's is —
    /// it is the clause whose absence would change the whole table.
    pub(crate) fn update_where(&mut self, table: TableRef) -> Result<StatementKind> {
        let edit = if self.eat_keyword(Keyword::Set) {
            let mut assignments = vec![self.assignment()?];
            while self.eat_punct(Punct::Comma) {
                assignments.push(self.assignment()?);
            }
            Edit::Fields(assignments)
        } else if self.eat_keyword(Keyword::Merge) {
            Edit::Merge(self.expression()?)
        } else {
            return Err(self.error_here(
                "`:` and one record's identity, or `SET` or `MERGE` and a `WHERE` — a whole value would replace every matched record",
            ));
        };
        self.expect_keyword(
            Keyword::Where,
            "`WHERE` and which records to change, or `:` and one record's identity",
        )?;
        let condition = self.condition()?;
        crate::parser::shape::no_fold(&condition)?;
        crate::parser::shape::check_several(&condition)?;
        let limit = self.set_bound()?;
        let expire = self.write_expiry()?;
        Ok(StatementKind::UpdateWhere {
            table,
            edit,
            condition: Box::new(condition),
            limit,
            expire,
            answer: self.answer(Keyword::Update)?,
        })
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

impl Parser<'_> {
    /// The read an `INSERT … SELECT` takes its rows from (ADR-0134 D7).
    ///
    /// Its projection names the values, one per column in the same order, and
    /// no `*`: a record's fields have no position to map a column to.
    fn insert_read(&mut self, columns: usize) -> Result<Select> {
        let opened = self.span_here();
        let read = self.select_statement()?;
        let reads = std::iter::once(&read).chain(read.combined.iter().map(|next| &*next.read));
        let names = |one: &Select| -> Vec<String> {
            one.projection
                .written()
                .iter()
                .map(|value| value.name.text.clone())
                .filter(|name| name != crate::HAVING_NAME)
                .collect()
        };
        let first = names(&read);
        for one in reads {
            // The rows are matched to the columns by the first read's names,
            // so a combined read answers under those names throughout.
            if names(one) != first {
                return Err(Error::UnexpectedToken {
                    expected: "reads answering under the same names, so each row's values reach \
                               the same columns",
                    found: "a combined read naming other values".to_owned(),
                    span: opened,
                });
            }
            let Projection::Values {
                everything: None,
                values,
            } = &one.projection
            else {
                return Err(Error::UnexpectedToken {
                    expected: "the values each row supplies, named one per column — not `*`",
                    found: "`*`".to_owned(),
                    span: opened,
                });
            };
            let found = values
                .iter()
                .filter(|value| value.name.text != crate::HAVING_NAME)
                .count();
            if found != columns {
                return Err(Error::InsertRowArity {
                    row: 1,
                    found,
                    expected: columns,
                    span: opened,
                });
            }
        }
        Ok(read)
    }
}
