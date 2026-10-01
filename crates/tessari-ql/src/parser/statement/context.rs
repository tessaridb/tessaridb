//! Statements about the session: INFO, USE and LET.

use super::Parser;

use crate::ast::{Expr, ExprKind, InfoSubject, Name, StatementKind};
use crate::error::Result;
use crate::token::{Keyword, Punct, Token};

/// The most ids one `INFO FOR VAULT … RECORDS` page may ask for — a bound, so
/// the answer is never a vault materialised whole (ADR-0092 D5).
const VAULT_PAGE_CEILING: u64 = 10_000;

impl Parser<'_> {
    /// `INFO FOR STORE` / `NAMESPACE` / `DATABASE` / `TABLE users` / `USER ada`
    ///
    /// # Only `INFO` is reserved
    ///
    /// `FOR` and `STORE` are read as contextual words, for the reason
    /// [`Parser::eat_word`] gives: reserving a word takes a perfectly good table
    /// and field name away from data that already exists. Nothing but `FOR` can
    /// stand after `INFO` and nothing but a subject after `FOR`, so nothing here
    /// is ambiguous. `INFO` itself has to be reserved, because it leads a
    /// statement and the dispatcher reads a keyword — and that costs `DEFINE
    /// TABLE info`, which is the price and is worth naming.
    ///
    /// A namespace and a database are the **selected** ones rather than named
    /// ones. A caller asking about another says `USE`, which is the tenancy
    /// question answered where the store already answers it, rather than a
    /// second path to the same check.
    pub(super) fn info_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        if !self.eat_word("for") {
            return Err(self.error_here("`FOR` and what to report on"));
        }
        let subject = match self.peek_keyword() {
            Some(Keyword::Namespace) => {
                self.advance();
                InfoSubject::Namespace
            }
            // A keyword since a reach could be granted; a bare word before
            // that, which is why this arm reads like the two beside it now.
            Some(Keyword::Store) => {
                self.advance();
                InfoSubject::Store
            }
            Some(Keyword::Database) => {
                self.advance();
                InfoSubject::Database
            }
            Some(Keyword::Table) => {
                self.advance();
                InfoSubject::Table(self.table_ref()?)
            }
            Some(Keyword::Graph) => {
                self.advance();
                InfoSubject::Graph(self.name()?)
            }
            // A keyword, unlike `VECTOR`, `GEO` and `VAULT` below, because
            // `DEFINE BUCKET` reserved the word before this subject existed —
            // so it is read here as one rather than as a bare word.
            Some(Keyword::Bucket) => {
                self.advance();
                InfoSubject::Bucket(self.name()?)
            }
            Some(Keyword::User) => {
                self.advance();
                InfoSubject::User(self.name()?)
            }
            // Before the `Keyword::User` arm cannot reach it: `USERS` is a word
            // and `USER` is a keyword, so the two never collide at the lexer.
            _ if self.eat_word("users") => InfoSubject::Users,
            // `ACCESS TO TABLE orders` — the object named the way every other
            // statement names one, so that a table whose name is a keyword is
            // reachable here for the same reason it is reachable anywhere else.
            _ if self.eat_word("access") => {
                self.expect_keyword(Keyword::To, "`TO` and the object to report on")?;
                self.expect_keyword(Keyword::Table, "`TABLE` and its name")?;
                InfoSubject::Access(self.table_ref()?)
            }
            _ if self.eat_word("node") => InfoSubject::Node,
            // Plural first, as with `USERS` above, so that reading this arm in
            // order tells you which of the two a bare word reaches.
            _ if self.eat_word("kafka") => {
                if self.eat_word("consumers") {
                    InfoSubject::Consumers
                } else {
                    self.expect_word("consumer", "`CONSUMER` or `CONSUMERS` after `KAFKA`")?;
                    InfoSubject::Consumer(self.name()?)
                }
            }
            _ if self.peek_word("consumer") || self.peek_word("consumers") => {
                return Err(self.error_here(
                    "`INFO FOR KAFKA CONSUMER` — the bare word now belongs to a \
                     queue's readers",
                ));
            }
            _ if self.eat_word("vector") => InfoSubject::Vector(self.name()?),
            _ if self.eat_keyword(Keyword::Search) => InfoSubject::Search(self.name()?),
            _ if self.eat_word("geo") => InfoSubject::Geo(self.name()?),
            _ if self.eat_word("vault") => {
                let table = self.table_ref()?;
                if self.eat_word("records") {
                    let after = self.after_anchor()?;
                    let limit = self.bound("limit")?;
                    if limit.is_some_and(|limit| limit > VAULT_PAGE_CEILING) {
                        return Err(self.error_here("a page of at most 10000 ids"));
                    }
                    InfoSubject::VaultRecords {
                        table,
                        after,
                        limit,
                    }
                } else {
                    InfoSubject::Vault(table.name)
                }
            }
            _ if self.eat_word("topic") => {
                if self.topic_consumer_follows(false) {
                    self.eat_word("consumer");
                    InfoSubject::TopicConsumer(self.name()?)
                } else {
                    InfoSubject::Topic(self.table_ref()?)
                }
            }
            _ if self.eat_word("recipients") => {
                self.expect_word("of", "`OF` and the record")?;
                InfoSubject::Recipients(self.record_target()?)
            }
            _ if self.eat_word("versions") => {
                self.expect_word("of", "`OF` and the record")?;
                InfoSubject::Versions(self.record_target()?)
            }
            _ if self.eat_word("history") => {
                self.expect_word("of", "`OF` and the record")?;
                InfoSubject::History(self.record_target()?)
            }
            _ if self.eat_word("audit") => InfoSubject::Audit(self.audited_actor()?),
            _ if self.eat_word("seal") => InfoSubject::Seal(if self.eat_word("of") {
                Some(self.name()?)
            } else {
                None
            }),
            _ => {
                // Every subject the arms above accept, and in their order, so
                // that adding an arm and forgetting this line is a visible
                // omission rather than an invisible one. `GRAPH` and
                // `RECIPIENTS` were missing from it for as long as they have
                // parsed (Q-428): a message that lists its options and gets the
                // list wrong is worse than one that lists none, because a caller
                // reads it as the whole truth and stops looking.
                return Err(self.error_here(
                    "`STORE`, `NAMESPACE`, `DATABASE`, `TABLE`, `GRAPH`, `BUCKET`, `USER`, `USERS`, `ACCESS`, `NODE`, `KAFKA CONSUMER`, `KAFKA CONSUMERS`, `VECTOR`, `GEO`, `VAULT`, `TOPIC`, `RECIPIENTS OF`, `VERSIONS OF`, `HISTORY OF`, `AUDIT` or `SEAL`",
                ));
            }
        };
        Ok(StatementKind::Info { subject })
    }

    /// The actor an `INFO FOR AUDIT BY …` narrows to, when one is named.
    ///
    /// Quoted or bare, for the reason a declared field name is: the trail holds
    /// an actor as a string it was handed, so a user whose name is a keyword
    /// would otherwise be the one credential the forensic question cannot be
    /// asked about — and the credential somebody named `PASSWORD` is not the
    /// one to lose.
    pub(super) fn audited_actor(&mut self) -> Result<Option<Name>> {
        if !self.eat_word("by") {
            return Ok(None);
        }
        if matches!(self.peek(), Some(Token::Str(_))) {
            let (text, span) = self.quoted_field_name()?;
            return Ok(Some(Name { text, span }));
        }
        self.name().map(Some)
    }

    /// `USE NAMESPACE prod DATABASE orders` — either part, in that order.
    pub(super) fn use_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        let mut namespace = None;
        let mut database = None;
        if self.eat_keyword(Keyword::Namespace) {
            namespace = Some(self.name()?);
        }
        if self.eat_keyword(Keyword::Database) {
            database = Some(self.name()?);
        }
        // Contextual like every other subject word in this file: `consumer` is
        // a perfectly ordinary column name in an application that has
        // customers, and reserving it here would reserve it everywhere.
        let consumer = if self.eat_word("consumer") {
            Some(self.consumer_name()?)
        } else {
            None
        };
        if namespace.is_none() && database.is_none() && consumer.is_none() {
            return Err(self.error_here("`NAMESPACE`, `DATABASE` or `CONSUMER` after `USE`"));
        }
        Ok(StatementKind::Use {
            namespace,
            database,
            consumer,
        })
    }

    /// `LET $recent = SELECT id FROM notes ORDER BY at DESC LIMIT 5`
    ///
    /// The name is a parameter token rather than an identifier, which is what
    /// makes a binding and a caller's value the same kind of thing everywhere
    /// below: `$recent` reads identically whether the script bound it or the
    /// caller supplied it, so nothing downstream has to know which happened.
    pub(super) fn let_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        let span = self.span_here();
        let Some(Token::Parameter(name)) = self.peek() else {
            return Err(self.error_here("a `$name` to bind after `LET`"));
        };
        let name = name.clone();
        self.advance();
        self.expect_punct(Punct::Equals, "`=` after the name to bind")?;
        Ok(StatementKind::Let {
            name,
            value: self.value_or_read()?,
            span,
        })
    }

    /// An expression, or a read written without parentheses.
    ///
    /// One rule rather than two spellings: a read needs parentheses where it
    /// sits **inside** a larger expression, because `IN (SELECT …)` has to say
    /// where the read stops. After `LET $x =` and after `RETURN` it runs to the
    /// end of the statement, so there is nothing for a parenthesis to
    /// disambiguate and requiring one would be ceremony.
    pub(super) fn value_or_read(&mut self) -> Result<Expr> {
        if self.peek_keyword() != Some(Keyword::Select) {
            return self.expression();
        }
        let start = self.span_here();
        let select = self.select_statement()?;
        let span = start.to(self.span_behind());
        Ok(Expr {
            kind: ExprKind::Select(Box::new(select)),
            span,
        })
    }
}
