//! Writes, drops and the checks that rebuild or verify.

mod edits;
use super::Parser;
use tessari_types::{IdentityKind, RecordId};

use crate::ast::{CreateTarget, Edit, Identity, StatementKind};
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
            _ if self.eat_word("topic") => {
                if self.topic_consumer_follows(false) {
                    self.eat_word("consumer");
                    Ok(StatementKind::DropTopicConsumer { name: self.name()? })
                } else {
                    Ok(StatementKind::DropTable {
                        table: self.table_ref()?,
                    })
                }
            }
            _ if self.eat_word("group") => self.drop_group(),
            _ if self.eat_word("series") => Ok(StatementKind::DropSeries { name: self.name()? }),
            _ if self.eat_word("rollup") => Ok(StatementKind::DropRollup { name: self.name()? }),
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
}
