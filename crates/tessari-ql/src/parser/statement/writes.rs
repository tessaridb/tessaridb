//! Writes, drops and the checks that rebuild or verify.

use super::Parser;
use tessari_types::{IdentityKind, RecordId};

use crate::ast::{
    Answer, Assignment, CreateTarget, Edit, Expr, Identity, RecordTarget, StatementKind,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct};

impl Parser<'_> {
    /// `REBUILD INDEX <name> ON <table>`
    ///
    /// `INDEX` is spelled out although nothing else can be rebuilt yet, because
    /// the alternative reads as though the table were the thing being rebuilt.
    pub(super) fn rebuild_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        self.expect_keyword(Keyword::Index, "`INDEX` and the index to rebuild")?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the index reads")?;
        Ok(StatementKind::RebuildIndex {
            name,
            table: self.table_ref()?,
        })
    }

    /// `CHECK TABLE <table>`
    ///
    /// `TABLE` is spelled out for the reason `REBUILD INDEX` spells its noun:
    /// nothing else is checkable yet, and without the noun the statement reads
    /// as though the table were being repaired rather than examined.
    pub(super) fn check_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        self.expect_keyword(Keyword::Table, "`TABLE` and the table to check")?;
        Ok(StatementKind::CheckTable {
            table: self.table_ref()?,
        })
    }

    pub(super) fn drop_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        match self.peek_keyword() {
            // A bucket is a table row carrying `bucket: true` — `DEFINE BUCKET`
            // reaches `define_table` — so the words undefine the same catalog
            // entry and differ only in which one the reader wrote.
            Some(Keyword::Table | Keyword::Space | Keyword::Bucket) => {
                self.advance();
                Ok(StatementKind::DropTable {
                    table: self.table_ref()?,
                })
            }
            Some(Keyword::User) => {
                self.advance();
                Ok(StatementKind::DropUser { name: self.name()? })
            }
            Some(Keyword::Analyzer) => {
                self.advance();
                Ok(StatementKind::DropAnalyzer { name: self.name()? })
            }
            Some(Keyword::Database) => {
                self.advance();
                Ok(StatementKind::DropDatabase { name: self.name()? })
            }
            Some(Keyword::Namespace) => {
                self.advance();
                Ok(StatementKind::DropNamespace { name: self.name()? })
            }
            Some(Keyword::Graph) => {
                self.advance();
                Ok(StatementKind::DropGraph { name: self.name()? })
            }
            Some(Keyword::Edge) => {
                self.advance();
                Ok(StatementKind::DropEdge { name: self.name()? })
            }
            Some(Keyword::Index) => {
                self.advance();
                let name = self.name()?;
                self.expect_keyword(Keyword::On, "`ON` and the table the index reads")?;
                Ok(StatementKind::DropIndex {
                    name,
                    table: self.table_ref()?,
                })
            }
            Some(Keyword::Field) => {
                self.advance();
                let name = self.name()?;
                self.expect_keyword(Keyword::On, "`ON` and the table the field is on")?;
                Ok(StatementKind::DropField {
                    name,
                    table: self.table_ref()?,
                })
            }
            // Contextual, for the reason `DEFINE KAFKA CONSUMER` is: `consumer` is a
            // plausible table in an application that has customers, and nothing
            // but a subject can stand here.
            _ if self.eat_word("kafka") => {
                self.expect_word("consumer", "`CONSUMER` after `KAFKA`")?;
                Ok(StatementKind::DropConsumer { name: self.name()? })
            }
            _ if self.peek_word("consumer") => Err(self.error_here(
                "`DROP KAFKA CONSUMER` — the bare word now belongs to a \
                 queue's readers",
            )),
            // Contextual for the same reason `consumer` is, and listed before
            // `node` so that reading these two in order tells you which of them
            // a bare word reaches.
            _ if self.eat_word("replica") => Ok(StatementKind::DropReplica { name: self.name()? }),
            // Contextual, as the word is everywhere else it appears.
            _ if self.eat_word("vector") => Ok(StatementKind::DropVector { name: self.name()? }),
            _ if self.eat_word("geo") => Ok(StatementKind::DropGeo { name: self.name()? }),
            _ if self.eat_word("vault") => Ok(StatementKind::DropVault { name: self.name()? }),
            _ if self.eat_word("queue") => Ok(StatementKind::DropQueue { name: self.name()? }),
            // A topic is a table carrying its declaration, as a space is, so the
            // word undefines the same catalog entry (G037).
            _ if self.eat_word("topic") => Ok(StatementKind::DropTable {
                table: self.table_ref()?,
            }),
            _ if self.eat_word("series") => Ok(StatementKind::DropSeries { name: self.name()? }),
            _ if self.eat_word("view") => Ok(StatementKind::DropView { name: self.name()? }),
            // Declined rather than missing, and it says so. `DEFINE NODE` writes
            // this process's own configuration outside the transaction, so its
            // inverse is an edit to a config file rather than a statement — and
            // a store that let one node undeclare another's identity over the
            // wire would be answering a question no reader asked it.
            _ if self.eat_word("node") => Err(self.error_here(
                "a node to undeclare — but a node is not undeclared by a \
                 statement: `DEFINE NODE` writes this process's own \
                 configuration, so change the configuration and restart it. \
                 `DROP REPLICA <name>` is the statement that stops counting \
                 another endpoint as a peer",
            )),
            _ => Err(self.error_here(
                "`TABLE`, `SPACE`, `BUCKET`, `INDEX`, `FIELD`, `ANALYZER`, \
                 `DATABASE`, `NAMESPACE`, `USER`, `CONSUMER` or `REPLICA`",
            )),
        }
    }

    /// The three statements shaped `<verb> <target> = <value>`.
    ///
    /// `CREATE` is the one whose target may stop at the table. The record it
    /// writes does not exist yet, so there is nothing for an address to point
    /// at, and the identity's absence is what asks the store to name it. The
    /// other verbs here change a record that is already there, where an address
    /// is the honest shape and stays required.
    pub(super) fn write_statement(&mut self, verb: Keyword) -> Result<StatementKind> {
        self.advance();
        let table = self.table_ref()?;
        if matches!(verb, Keyword::Create) && !self.at_punct(Punct::Colon) {
            self.expect_punct(Punct::Equals, "`=` and the value to write")?;
            let value = self.expression()?;
            return Ok(StatementKind::Create {
                target: CreateTarget::Generated(table),
                value,
                answer: self.answer(verb)?,
            });
        }
        let target = self.record_target_after(table)?;
        // `SET` is a key-value verb elsewhere and a clause here, which is the
        // trick this grammar already plays with `ORDER`, `FETCH` and `VECTOR`:
        // nothing but a clause can stand in this position, so nothing is
        // ambiguous, and a field called `set` keeps working.
        // `UPDATE` and `UPSERT` change a record, so both take the three edit
        // shapes. `CREATE` and `SET` write a whole value and take none of them.
        let edits = matches!(verb, Keyword::Update | Keyword::Upsert);
        if edits && self.eat_keyword(Keyword::Set) {
            let mut assignments = vec![self.assignment()?];
            while self.eat_punct(Punct::Comma) {
                assignments.push(self.assignment()?);
            }
            let edit = Edit::Fields(assignments);
            let condition = self.edit_condition(verb)?;
            return Ok(Self::changed(
                verb,
                target,
                edit,
                condition,
                self.answer(verb)?,
            ));
        }
        if edits && self.eat_keyword(Keyword::Merge) {
            // The **value** position, unlike `SET`'s right-hand sides: this is
            // one whole object standing for the change, not a route computed
            // from the record it is changing.
            let edit = Edit::Merge(self.expression()?);
            let condition = self.edit_condition(verb)?;
            return Ok(Self::changed(
                verb,
                target,
                edit,
                condition,
                self.answer(verb)?,
            ));
        }
        self.expect_punct(Punct::Equals, "`=` and the value to write")?;
        let value = self.expression()?;
        Ok(match verb {
            Keyword::Update | Keyword::Upsert => {
                let condition = self.edit_condition(verb)?;
                Self::changed(
                    verb,
                    target,
                    Edit::Whole(value),
                    condition,
                    self.answer(verb)?,
                )
            }
            Keyword::Set => self.set_statement(target, value)?,
            _ => StatementKind::Create {
                target: CreateTarget::Named(target),
                value,
                answer: self.answer(verb)?,
            },
        })
    }

    /// The identities after `SPLIT AT`, as written and in the order written.
    ///
    /// Each is read by the same `record_id` a `table:id` target uses, so a point
    /// is spelled exactly as the record it bounds would be addressed. A
    /// parameter is refused here rather than bound later: a shard boundary is
    /// read in the script that declared it.
    pub(super) fn split_points(&mut self) -> Result<Vec<RecordId>> {
        let mut points = Vec::new();
        loop {
            let at = self.span_here();
            match self.record_id(at)? {
                Identity::Fixed(point) => points.push(point),
                Identity::Parameter(name) => {
                    return Err(Error::SplitPointIsNotWritten {
                        name,
                        span: at.to(self.span_behind()),
                    });
                }
            }
            if !self.eat_punct(Punct::Comma) {
                return Ok(points);
            }
        }
    }

    /// The word after `IDENTITY`.
    ///
    /// `uuid` is a keyword — it already stands in `users:uuid '…'` — so it is
    /// eaten as one rather than read as a name, which is why this is not a bare
    /// [`IdentityKind::parse`] over the next identifier.
    ///
    /// An unrecognised word is refused and never defaulted: a table declared
    /// under a scheme this build does not know would otherwise start naming
    /// records with a counter while its author believed otherwise.
    pub(super) fn identity_kind(&mut self) -> Result<IdentityKind> {
        if self.eat_keyword(Keyword::Uuid) {
            return Ok(IdentityKind::Uuid);
        }
        let word = self.name()?;
        IdentityKind::parse(&word.text.to_ascii_lowercase()).ok_or(Error::UnknownIdentityKind {
            word: word.text,
            span: word.span,
        })
    }

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
    pub(super) fn insert_statement(&mut self) -> Result<StatementKind> {
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
    pub(super) fn answer(&mut self, verb: Keyword) -> Result<Answer> {
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
    pub(super) fn changed(
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
    pub(super) fn edit_condition(&mut self, verb: Keyword) -> Result<Option<Expr>> {
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
    pub(super) fn assignment(&mut self) -> Result<Assignment> {
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
