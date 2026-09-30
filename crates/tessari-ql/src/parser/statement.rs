//! One statement at a time.

use super::Parser;
use tessari_types::{Assertion, Path, Step};

use crate::ast::{FieldPath, Name, Statement, StatementKind, Written};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

mod access;
mod cluster;
mod context;
mod define;
mod fields;
mod queues;
mod relations;
mod select;
mod topic_consumer;
mod vaults;
mod writes;

/// What a field declaration says after its type.
///
/// A struct rather than a four-value tuple so that the two call sites cannot
/// bind them in the wrong order — `analyzer` and a `default` that happens to be
/// a name would both compile.
#[derive(Default)]
struct FieldOptions {
    required: bool,
    secret: bool,
    default: Option<Written>,
    analyzer: Option<Name>,
    assert: Option<Assertion>,
}

/// The parameter name that reads as this node in a `FROM`.
///
/// Spelled with a sigil rather than reserved as a word, so that `node` stays an
/// ordinary table and field name for data that already uses it.
const NODE_SOURCE: &str = "node";

/// Which of the two tables a join key names, and the route below it.
///
/// The root is the table; what is left is a route into one of its records. A
/// first step that is a position rather than a field means the author indexed
/// the table itself, which is not a thing a table is.
fn side_of(key: &FieldPath, sides: &[String; 2]) -> Result<(usize, FieldPath)> {
    let Some(side) = sides.iter().position(|name| name == key.path.root()) else {
        return Err(Error::NotASideOfTheJoin {
            root: key.path.root().to_owned(),
            left: sides[0].clone(),
            right: sides[1].clone(),
            span: key.span,
        });
    };
    let mut steps = key.path.steps().iter().cloned();
    let Some(Step::Field(root)) = steps.next() else {
        return Err(Error::JoinKeyIsNotAField {
            root: key.path.root().to_owned(),
            span: key.span,
        });
    };
    Ok((
        side,
        FieldPath {
            path: Path::new(root, steps.collect()),
            span: key.span,
        },
    ))
}

impl Parser<'_> {
    pub(super) fn statement(&mut self) -> Result<Statement> {
        let start = self.span_here();
        let kind = match self.peek_keyword() {
            Some(Keyword::Use) => self.use_statement()?,
            Some(Keyword::Define) => self.define_statement()?,
            Some(Keyword::Drop) => self.drop_statement()?,
            Some(Keyword::Alter) => self.alter_statement()?,
            Some(Keyword::Rebuild) => self.rebuild_statement()?,
            Some(Keyword::Check) => self.check_statement()?,
            Some(Keyword::Grant) => self.grant_statement(true)?,
            Some(Keyword::Revoke) => self.grant_statement(false)?,
            Some(Keyword::Create) => self.write_statement(Keyword::Create)?,
            Some(Keyword::Insert) => self.insert_statement()?,
            Some(Keyword::Update) => self.write_statement(Keyword::Update)?,
            Some(Keyword::Upsert) => self.write_statement(Keyword::Upsert)?,
            Some(Keyword::Throw) => {
                self.advance();
                // The value position: the message is a value, and a bare name
                // here would be a table exactly as it is in every other one.
                StatementKind::Throw {
                    value: self.expression()?,
                }
            }
            Some(Keyword::Set) => self.write_statement(Keyword::Set)?,
            Some(Keyword::Let) => self.let_statement()?,
            Some(Keyword::Return) => {
                self.advance();
                StatementKind::Return {
                    value: self.value_or_read()?,
                }
            }
            Some(Keyword::Select) => StatementKind::Select(Box::new(self.select_statement()?)),
            Some(Keyword::Explain) => {
                self.advance();
                // Only a read has a plan to describe. A write's cost is its
                // index maintenance, which is a different report rather than
                // this one wearing the same word.
                if self.peek_keyword() != Some(Keyword::Select) {
                    return Err(self.error_here("`SELECT` and the read to explain"));
                }
                StatementKind::Explain(Box::new(self.select_statement()?))
            }
            Some(Keyword::Info) => self.info_statement()?,
            Some(Keyword::Delete) => {
                self.advance();
                // `FROM` is what tells the two forms apart, and it is required
                // for the conditional one: `DELETE readings WHERE …` would read
                // as a table name where an identity belongs, and a statement
                // that removes rows should not be one word away from a typo.
                if self.eat_keyword(Keyword::From) {
                    let table = self.table_ref()?;
                    // `DELETE FROM events:1000..2000` is the retention form and
                    // reads exactly as the span a `SELECT` takes, because it
                    // removes the records that read would have answered with.
                    if self.peek() == Some(&Token::Punct(Punct::Colon)) {
                        let record = self.record_target_after(table)?;
                        // A span and nothing else. `DELETE FROM events:1000`
                        // would name one record in the form reserved for a set,
                        // and `DELETE events:1000` already says that — so the
                        // missing `..` is refused rather than read as either.
                        let Some(inclusive) = self.range_bound() else {
                            return Err(self.error_here("`..` or `..=` and the end of the span"));
                        };
                        let at = self.span_here();
                        let upper = self.record_id(at)?;
                        StatementKind::DeleteSpan {
                            span: record.span.to(self.span_behind()),
                            table: record.table,
                            lower: record.id,
                            upper,
                            inclusive,
                            limit: self.delete_bound()?,
                        }
                    } else {
                        self.expect_keyword(Keyword::Where, "`WHERE` and what to remove")?;
                        let condition = self.condition()?;
                        super::shape::no_fold(&condition)?;
                        super::shape::check_several(&condition)?;
                        StatementKind::DeleteWhere {
                            table,
                            condition: Box::new(condition),
                            limit: self.delete_bound()?,
                        }
                    }
                } else {
                    let target = self.record_target()?;
                    // An arrow after the target says the subject is an edge and
                    // not the record just named. Decided on the token rather
                    // than on a lookahead over the whole clause: the two forms
                    // diverge here and nowhere else.
                    if self.eat_punct(Punct::ArrowRight) {
                        let edges = self.table_ref()?;
                        self.expect_punct(Punct::ArrowRight, "`->` and the record at the far end")?;
                        let to = self.record_target()?;
                        StatementKind::DeleteEdge {
                            from: target,
                            edges,
                            to,
                            answer: self.answer(Keyword::Delete)?,
                        }
                    } else {
                        StatementKind::Delete {
                            target,
                            answer: self.answer(Keyword::Delete)?,
                        }
                    }
                }
            }
            Some(Keyword::Get) => {
                self.advance();
                StatementKind::Get {
                    target: self.record_target()?,
                }
            }
            Some(Keyword::Put) => {
                self.advance();
                let target = self.record_target()?;
                // Before the `=`, because it qualifies the target rather than
                // the value: `PUT media:'/x' START 1024 = 0x…` writes those
                // bytes at that offset.
                let start = self.bound("start")?;
                self.expect_punct(Punct::Equals, "`=` and the file's bytes")?;
                StatementKind::Put {
                    target,
                    start,
                    value: self.expression()?,
                }
            }
            Some(Keyword::Read)
                if matches!(self.peek_ahead(1), Some(Token::Keyword(Keyword::From))) =>
            {
                self.advance();
                self.read_topic()?
            }
            Some(Keyword::Read) => {
                self.advance();
                let target = self.record_target()?;
                // The same two words a bounded read of rows uses, meaning the
                // same two things over bytes: skip this many, take this many.
                let start = self.bound("start")?;
                let limit = self.bound("limit")?;
                StatementKind::Read {
                    target,
                    start,
                    limit,
                }
            }
            Some(Keyword::Backup) => {
                self.advance();
                // `STATE` is contextual, as every word a statement's head adds
                // is: it is an ordinary field name everywhere else, and after
                // `BACKUP` nothing but this word or `FROM` can stand.
                if self.eat_word("state") {
                    if self.eat_keyword(Keyword::From) {
                        return Err(self.error_here(
                            "the end of the statement; a snapshot is one moment, and `FROM` \
                             names a position in a log",
                        ));
                    }
                    StatementKind::Backup {
                        from: None,
                        form: crate::ast::BackupForm::State,
                    }
                } else {
                    // `FROM` reads as it does everywhere else — where the answer
                    // starts — and leaving it out means the whole log, which is
                    // what `write_from(.., 1)` already is.
                    let from = if self.eat_keyword(Keyword::From) {
                        let expected = "the sequence the backup starts at";
                        let Some(Token::Number(tessari_types::Number::Integer(held))) = self.peek()
                        else {
                            return Err(self.error_here(expected));
                        };
                        let held = u64::try_from(*held).map_err(|_| self.error_here(expected))?;
                        self.advance();
                        Some(held)
                    } else {
                        None
                    };
                    StatementKind::Backup {
                        from,
                        form: crate::ast::BackupForm::Log,
                    }
                }
            }
            Some(Keyword::Del) => {
                self.advance();
                StatementKind::Del {
                    target: self.record_target()?,
                }
            }
            Some(Keyword::Relate) => self.relate_statement()?,
            Some(Keyword::Keys) => self.keys_statement()?,
            Some(Keyword::Begin) => {
                self.advance();
                StatementKind::Begin
            }
            Some(Keyword::Commit) => {
                self.advance();
                StatementKind::Commit
            }
            Some(Keyword::Cancel) => {
                self.advance();
                StatementKind::Cancel
            }
            Some(Keyword::Verify) => {
                self.advance();
                StatementKind::Verify
            }
            // Three contextual verbs, for the reason `DEFINE VAULT` is
            // contextual: `reveal`, `seal` and `unseal` are ordinary column
            // names, and reserving them here would reserve them everywhere —
            // including in the vault whose fields somebody is declaring. Nothing
            // but a verb can stand at the head of a statement, so nothing is
            // ambiguous, and each arm has already consumed its word.
            // Two more contextual verbs, and here the reason is the strongest
            // it gets: `claim` and `release` are ordinary column names in
            // exactly the kind of application that wants a queue — a task
            // tracker's own table has a `claim` on it — so reserving them here
            // would take them away from the schema the word exists to serve.
            _ if self.eat_word("claim") => self.claim_statement(start)?,
            _ if self.eat_word("ack") => self.ack_statement()?,
            _ if self.eat_word("nack") => self.nack_statement()?,
            // `expire` and `persist` for the same reason as `claim`: a cache's
            // own tables are where an `expire` column lives (G035).
            _ if self.eat_word("expire") => self.expire_statement()?,
            _ if self.eat_word("persist") => self.persist_statement()?,
            _ if self.eat_word("incr") => self.incr_statement()?,
            _ if self.eat_word("release") => self.release_statement(start)?,
            _ if self.eat_word("reveal") => self.reveal_statement(start)?,
            _ if self.eat_word("add") => self.add_recipient_statement(start)?,
            _ if self.eat_word("remove") => self.remove_recipient_statement(start)?,
            _ if self.eat_word("unseal") => self.unseal_statement(start)?,
            _ if self.eat_word("seal") => {
                self.expect_vault_word("`VAULT`")?;
                StatementKind::SealVault {
                    span: start.to(self.span_behind()),
                }
            }
            _ => return Err(self.error_here("a statement")),
        };
        Ok(Statement {
            kind,
            span: start.to(self.span_behind()),
        })
    }
}
