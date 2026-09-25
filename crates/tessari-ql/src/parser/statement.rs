//! One statement at a time.

use core::num::NonZeroU32;

use super::Parser;
use tessari_types::{
    Assertion, ConflictPolicy, Duration, FieldKind, Filter, IdentityKind, Number, Path, RecordId,
    Replication, ReplicationClass, Step, parse_uuid,
};

use crate::ast::{
    Answer, Approximation, Assignment, ColumnDeclaration, ConsumerSource, CreateTarget, Direction,
    EdgeClause, EdgeEndpoints, EdgeOrdering, Edit, Expr, ExprKind, FieldMapping, FieldPath, Hop,
    Identity, InfoSubject, JoinSide, Name, OnFailure, Password, Projection, RangeExpr, ReachRef,
    RecordTarget, Select, Source, Statement, StatementKind, TableChange, TableRef, UserChange,
    UserGrant, Written,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Span, Token};

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
                // `FROM` reads as it does everywhere else — where the answer
                // starts — and leaving it out means the whole log, which is what
                // `write_from(.., 1)` already is.
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
                StatementKind::Backup { from }
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

    /// `REVEAL password FROM team:github` · `REVEAL * FROM team:github`
    ///
    /// A field list or `*`, then one record. There is no `WHERE` and no `ORDER
    /// BY`, and their absence is the feature: a filter over a secret is an
    /// oracle answering one bit per statement, and a verb with nowhere to put
    /// one cannot be talked into accepting one later by a clause somebody adds
    /// for a different reason.
    fn reveal_statement(&mut self, start: Span) -> Result<StatementKind> {
        let mut fields = Vec::new();
        if !self.eat_punct(Punct::Star) {
            loop {
                // The same reader the declaration uses. A field that can only be
                // declared by quoting its name has to be openable by quoting it
                // too, or `REVEAL` is the one statement that cannot name what
                // `DEFINE FIELD` just created.
                fields.push(self.declared_field_name()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        self.expect_keyword(Keyword::From, "`FROM` and the record to open")?;
        let target = self.record_target()?;
        Ok(StatementKind::Reveal {
            target,
            fields,
            span: start.to(self.span_behind()),
        })
    }

    /// `ADD RECIPIENT 'ops-escrow' TO team:github KEY $wrapped`
    ///
    /// The material is an **expression** where the passphrase below is a bare
    /// literal, and the difference is deliberate rather than inconsistent. A
    /// passphrase is a secret this store must never let through the evaluator; a
    /// recipient's material is the caller's own ciphertext, and the caller that
    /// computed it client-side needs to hand it over as a parameter rather than
    /// format it into a statement as hex.
    fn add_recipient_statement(&mut self, start: Span) -> Result<StatementKind> {
        self.expect_word("recipient", "`RECIPIENT`")?;
        let recipient = self.expression()?;
        self.expect_keyword(Keyword::To, "`TO` and the record")?;
        let target = self.record_target()?;
        self.expect_word("key", "`KEY` and the material to store")?;
        let material = self.expression()?;
        Ok(StatementKind::AddRecipient {
            target,
            recipient,
            material,
            span: start.to(self.span_behind()),
        })
    }

    /// `REMOVE RECIPIENT 'ops-escrow' FROM team:github`
    fn remove_recipient_statement(&mut self, start: Span) -> Result<StatementKind> {
        self.expect_word("recipient", "`RECIPIENT`")?;
        let recipient = self.expression()?;
        self.expect_keyword(Keyword::From, "`FROM` and the record")?;
        let target = self.record_target()?;
        Ok(StatementKind::RemoveRecipient {
            target,
            recipient,
            span: start.to(self.span_behind()),
        })
    }

    /// A contextual word this statement requires.
    /// `REPLICATION NONE` or `REPLICATION FACTOR 3`, when one stands here.
    ///
    /// `REPLICATION` and `FACTOR` are read as **contextual words** rather than
    /// added to the keyword table, which is not a shortcut: `DEFINE NAMESPACE`
    /// appears 333 times across this engine and four sibling repositories, and
    /// reserving a word retroactively refuses every script that used it as a
    /// name. Nothing here is ambiguous — a bare word after a namespace's name
    /// has no other reading — so the reservation would buy nothing and cost the
    /// corpus.
    ///
    /// Answers `None` when no clause stands here, which is what a namespace
    /// that said nothing is; see [`StatementKind::DefineNamespace`] for why
    /// that is not [`Replication::None`].
    fn replication_clause(&mut self) -> Result<Option<Replication>> {
        if !self.eat_word("replication") {
            return Ok(None);
        }
        if self.eat_keyword(Keyword::None) {
            return Ok(Some(Replication::None));
        }
        self.expect_word("factor", "`NONE` or `FACTOR` and a count")?;
        // `whole_number` already refuses zero and says so at the author's own
        // span, which is the answer this clause needs: a factor of zero is not
        // a policy, it says the data is kept nowhere.
        let factor = NonZeroU32::new(self.whole_number("a replication factor of at least one")?)
            .ok_or_else(|| self.error_here("a replication factor of at least one"))?;
        Ok(Some(Replication::Factor(factor)))
    }

    /// `MULTI MASTER` or `SINGLE LEADER`, the clause that says how many writers
    /// a namespace admits (G027 S2.1).
    ///
    /// Four contextual words rather than four keywords, for
    /// [`Self::replication_clause`]'s reason and with more force: `MASTER`,
    /// `LEADER`, `SINGLE` and `MULTI` are ordinary English nouns that a corpus
    /// of 333 `DEFINE NAMESPACE` statements across this engine and four sibling
    /// repositories may well already use as names, and reserving one
    /// retroactively refuses every script that did. Two words rather than one
    /// because the phrase is what an operator already calls the thing, so the
    /// clause they type is the phrase `INFO FOR` will read back to them.
    ///
    /// Answers `None` when no clause stands here. That namespace said nothing,
    /// which reads as single-leader everywhere and is deliberately not
    /// [`ReplicationClass::SingleLeader`] — see
    /// [`StatementKind::DefineNamespace`].
    fn replication_class_clause(&mut self) -> Result<Option<ReplicationClass>> {
        if self.eat_word("multi") {
            self.expect_word("master", "`MASTER` after `MULTI`")?;
            return Ok(Some(ReplicationClass::MultiMaster));
        }
        if self.eat_word("single") {
            self.expect_word("leader", "`LEADER` after `SINGLE`")?;
            return Ok(Some(ReplicationClass::SingleLeader));
        }
        Ok(None)
    }

    fn expect_word(&mut self, word: &str, expected: &'static str) -> Result<()> {
        if self.eat_word(word) {
            return Ok(());
        }
        Err(self.error_here(expected))
    }

    /// `UNSEAL VAULT WITH '…'`
    ///
    /// The passphrase is a **string literal** and nothing else — not an
    /// expression, not a parameter, not a name. An expression here would put a
    /// secret through the evaluator, where it could be concatenated into a
    /// message, compared with `=`, or returned by the very statement that read
    /// it; a literal goes from the lexer to the key derivation and nowhere else.
    fn unseal_statement(&mut self, start: Span) -> Result<StatementKind> {
        self.expect_vault_word("`VAULT`")?;
        if !self.eat_word("with") {
            return Err(self.error_here("`WITH` and the passphrase"));
        }
        let Some(Token::Str(passphrase)) = self.peek() else {
            // The expectation names the shape and never what stands there. Every
            // other parse error in this file quotes the token it found, and this
            // is the one position where the token is the secret.
            return Err(self.error_here("a quoted passphrase"));
        };
        let passphrase = passphrase.clone();
        self.advance();
        Ok(StatementKind::UnsealVault {
            passphrase,
            span: start.to(self.span_behind()),
        })
    }

    /// The word `VAULT` after `SEAL` or `UNSEAL`, which is contextual too.
    fn expect_vault_word(&mut self, expected: &'static str) -> Result<()> {
        if self.eat_word("vault") {
            return Ok(());
        }
        Err(self.error_here(expected))
    }

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
    fn info_statement(&mut self) -> Result<StatementKind> {
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
            _ if self.eat_word("geo") => InfoSubject::Geo(self.name()?),
            _ if self.eat_word("vault") => InfoSubject::Vault(self.name()?),
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
            _ => {
                // Every subject the arms above accept, and in their order, so
                // that adding an arm and forgetting this line is a visible
                // omission rather than an invisible one. `GRAPH` and
                // `RECIPIENTS` were missing from it for as long as they have
                // parsed (Q-428): a message that lists its options and gets the
                // list wrong is worse than one that lists none, because a caller
                // reads it as the whole truth and stops looking.
                return Err(self.error_here(
                    "`STORE`, `NAMESPACE`, `DATABASE`, `TABLE`, `GRAPH`, `BUCKET`, `USER`, `USERS`, `ACCESS`, `NODE`, `KAFKA CONSUMER`, `KAFKA CONSUMERS`, `VECTOR`, `GEO`, `VAULT`, `RECIPIENTS OF`, `VERSIONS OF`, `HISTORY OF` or `AUDIT`",
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
    fn audited_actor(&mut self) -> Result<Option<Name>> {
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
    fn use_statement(&mut self) -> Result<StatementKind> {
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

    /// The quoted name a session claims under.
    ///
    /// A literal rather than an identifier: it is the client's own string, not a
    /// catalog object, and nothing declares it first.
    fn consumer_name(&mut self) -> Result<String> {
        let Some(Token::Str(name)) = self.peek() else {
            return Err(self.error_here("a quoted consumer name"));
        };
        let name = name.clone();
        self.advance();
        if name.is_empty() {
            // An empty name would be a consumer that reads as *nobody said*,
            // which is what an ABSENT `claimed_by` already means. Two spellings
            // of one state is how a reader ends up asking which was meant.
            return Err(self.error_here("a consumer name with something in it"));
        }
        Ok(name)
    }

    /// `RELEASE jobs:7` · `RELEASE ALL FROM jobs [FOR CONSUMER 'billing']`
    fn release_statement(&mut self, start: Span) -> Result<StatementKind> {
        if !self.eat_word("all") {
            let target = self.record_target()?;
            return Ok(StatementKind::Release {
                target,
                consumer: self.for_consumer()?,
                span: start.to(self.span_behind()),
            });
        }
        self.expect_keyword(Keyword::From, "`FROM` and the queue to release")?;
        let table = self.table_ref()?;
        let consumer = self.for_consumer()?;
        Ok(StatementKind::ReleaseAll {
            table,
            consumer,
            span: start.to(self.span_behind()),
        })
    }

    /// The optional `FOR CONSUMER '<name>'` both release forms accept.
    ///
    /// `for` is read as a contextual word, exactly as `INFO FOR` reads it, so
    /// neither `for` nor `consumer` is taken away from an application's own
    /// schema. One function rather than two copies, because the two statements
    /// mean the same thing by it: *the group's hold, not this instance's*.
    fn for_consumer(&mut self) -> Result<Option<String>> {
        if !self.eat_word("for") {
            return Ok(None);
        }
        self.expect_word("consumer", "`CONSUMER` and a quoted name after `FOR`")?;
        Ok(Some(self.consumer_name()?))
    }

    /// `LET $recent = SELECT id FROM notes ORDER BY at DESC LIMIT 5`
    ///
    /// The name is a parameter token rather than an identifier, which is what
    /// makes a binding and a caller's value the same kind of thing everywhere
    /// below: `$recent` reads identically whether the script bound it or the
    /// caller supplied it, so nothing downstream has to know which happened.
    fn let_statement(&mut self) -> Result<StatementKind> {
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
    fn value_or_read(&mut self) -> Result<Expr> {
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

    fn define_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        match self.peek_keyword() {
            Some(Keyword::Namespace) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // Read in clause order, and the two are read in separate
                // statements rather than inside the struct literal because
                // field initialisers are evaluated in source order and a later
                // reordering of the fields would silently reorder the grammar.
                let replication = self.replication_clause()?;
                let class = self.replication_class_clause()?;
                Ok(StatementKind::DefineNamespace {
                    name,
                    if_not_exists,
                    replication,
                    class,
                })
            }
            Some(Keyword::Database) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineDatabase {
                    name: self.name()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Table) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // The columns come before the flags rather than in the same
                // order-free loop: they are the subject of the statement and
                // the flags are adjectives on it, and `DEFINE TABLE t
                // SCHEMAFULL (…)` reads as though the parentheses qualified
                // `SCHEMAFULL`.
                let columns = self.columns()?;
                // Either marker, in either order, and neither twice. Order-free
                // because there is no reading under which one has to precede the
                // other, and a grammar that insisted would only be remembered
                // wrong.
                //
                // A declared table is **strict by default**, so `schemafull`
                // starts true and `SCHEMALESS` is what turns it off. The pair is
                // read as two words rather than one optional word, because a
                // script that says which reading it wants keeps saying it after
                // the default moves again.
                let mut strictness: Option<bool> = None;
                let mut edge: Option<EdgeClause> = None;
                let mut identity: Option<IdentityKind> = None;
                let mut graph: Option<Name> = None;
                let mut conflict: Option<ConflictPolicy> = None;
                let mut split: Option<Vec<RecordId>> = None;
                loop {
                    if strictness.is_none() && self.eat_keyword(Keyword::Schemafull) {
                        strictness = Some(true);
                    } else if strictness.is_none() && self.eat_keyword(Keyword::Schemaless) {
                        strictness = Some(false);
                    } else if edge.is_none() && self.eat_keyword(Keyword::Edge) {
                        edge = Some(self.edge_clause()?);
                    } else if identity.is_none() && self.eat_word("identity") {
                        identity = Some(self.identity_kind()?);
                    } else if graph.is_none() && self.eat_keyword(Keyword::In) {
                        graph = Some(self.name()?);
                    // `LAST WRITER WINS` / `REFUSE CONFLICTS` — contextual
                    // words, reserving nothing, for the reason
                    // `replication_class_clause` gives about `MULTI MASTER`:
                    // `last`, `wins`, `refuse` and `conflicts` are ordinary
                    // English that a stored script may already use as a name,
                    // and reserving one retroactively refuses every script that
                    // did. The phrase is what an operator already calls the
                    // thing, so what they type is what `INFO FOR` reads back.
                    } else if conflict.is_none() && self.eat_word("last") {
                        self.expect_word("writer", "`WRITER` after `LAST`")?;
                        self.expect_word("wins", "`WINS` after `LAST WRITER`")?;
                        conflict = Some(ConflictPolicy::LastWriterWins);
                    } else if conflict.is_none() && self.eat_word("refuse") {
                        self.expect_word("conflicts", "`CONFLICTS` after `REFUSE`")?;
                        conflict = Some(ConflictPolicy::Refuse);
                    // `SPLIT AT` — contextual words for the reason the conflict
                    // phrase gives: `split` and `at` are ordinary English a
                    // stored script may already use as names.
                    } else if split.is_none() && self.eat_word("split") {
                        self.expect_word("at", "`AT` and the identity a shard begins at")?;
                        split = Some(self.split_points()?);
                    } else {
                        break;
                    }
                }
                // A table with no columns has nothing to be strict about, and
                // the reader who wrote it wanted the other word. Refused rather
                // than quietly read as lenient, and the refusal names the word,
                // because "not allowed" without "write this instead" turns a
                // one-word fix into a search through the specification.
                //
                // An edge table is the exception, and it is not a special case
                // so much as the rule read properly: it declares no columns
                // because nobody writes `out` and `in` by hand, so it is not a
                // declaration with nothing in it — it is one whose fields the
                // store supplies. It keeps the lenient reading it had, because
                // an edge carries properties and none of them were ever
                // declared here.
                if columns.is_empty() && strictness.is_none() && edge.is_none() && graph.is_none() {
                    return Err(Error::TableWithoutColumns {
                        name: name.text.clone(),
                        span: name.span,
                    });
                }
                let schemafull = strictness.unwrap_or(!columns.is_empty());
                Ok(StatementKind::DefineTable {
                    name,
                    columns,
                    schemafull,
                    edge,
                    identity: identity.unwrap_or_default(),
                    graph,
                    split: split.unwrap_or_default(),
                    conflict,
                    if_not_exists,
                })
            }
            Some(Keyword::Space) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineSpace {
                    name: self.name()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Graph) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineGraph {
                    name: self.name()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Edge) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // The graph is required and comes first, because an edge kind
                // that did not name one would be an edge table under a different
                // word — and the adjacency it writes has nowhere to live without
                // a graph id above the node.
                self.expect_keyword(Keyword::In, "`IN` and the graph the edge belongs to")?;
                let graph = self.name()?;
                self.expect_keyword(Keyword::From, "`FROM` and the table the edge leaves")?;
                let from = self.name()?;
                self.expect_keyword(Keyword::To, "`TO` and the table the edge reaches")?;
                let to = self.name()?;
                Ok(StatementKind::DefineEdge {
                    name,
                    graph,
                    from,
                    to,
                    if_not_exists,
                })
            }
            Some(Keyword::Bucket) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                Ok(StatementKind::DefineBucket {
                    name,
                    max: self.byte_ceiling()?,
                    if_not_exists,
                })
            }
            Some(Keyword::Collection) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                let name = self.name()?;
                // After the name, where the table spelling also takes it. A
                // collection has no strictness word and no columns, so this is
                // the whole of what follows one.
                let identity = if self.eat_word("identity") {
                    self.identity_kind()?
                } else {
                    IdentityKind::default()
                };
                Ok(StatementKind::DefineCollection {
                    name,
                    identity,
                    if_not_exists,
                })
            }
            Some(Keyword::Index) => self.define_index(),
            Some(Keyword::Field) => self.define_field(),
            Some(Keyword::Analyzer) => self.define_analyzer(),
            Some(Keyword::User) => self.define_user(),
            // `NODE` and `REPLICA` are read as contextual words, for the reason
            // `INFO FOR STORE` gives: reserving a word takes a perfectly good
            // table and field name away from data that already exists, and
            // `DEFINE TABLE node` is not a name to spend. Nothing but a subject
            // can stand after `DEFINE`, so nothing here is ambiguous — and both
            // arms consume their word, so neither may `advance` again.
            _ if self.eat_word("node") => self.define_node(),
            _ if self.eat_word("replica") => self.define_replica(),
            // And a third contextual subject, on the same reasoning: `failover`
            // is a perfectly good name for a table somebody's data already uses.
            _ if self.eat_word("failover") => self.define_failover(),
            // `KAFKA` qualifies the word rather than replacing it, and it is
            // contextual like every other subject here — special after `DEFINE`
            // and an ordinary identifier everywhere else, so a table called
            // `kafka` is still spellable.
            _ if self.eat_word("kafka") => {
                self.expect_word("consumer", "`CONSUMER` after `KAFKA`")?;
                self.define_consumer()
            }
            // The bare spelling is REFUSED rather than accepted, because the
            // word is being given to the queue: a `DEFINE CONSUMER` that kept
            // working would mean broker ingestion today and a claimant identity
            // later, and nothing in the statement would say which was meant.
            _ if self.peek_word("consumer") => Err(self.error_here(
                "`DEFINE KAFKA CONSUMER` — broker ingestion names its broker, \
                 and `CONSUMER` alone now belongs to a queue's readers",
            )),
            // Contextual for the reason `DEFINE INDEX … VECTOR` already gives:
            // a field called `vector` in a database of embeddings is not a name
            // to take away, and taking it away here would take it away
            // everywhere, since a reserved word is reserved in every position.
            _ if self.eat_word("vector") => self.define_vector(),
            // Contextual for the same reason, and with more at stake: `geo` is a
            // perfectly ordinary column name, and reserving it here would
            // reserve it everywhere.
            _ if self.eat_word("geo") => self.define_geo(),
            // Contextual for the same reason again, and here the reason is
            // strongest: `vault` is a perfectly ordinary table name in a
            // password manager's own schema, which is precisely the kind of
            // application this word exists for.
            _ if self.eat_word("vault") => self.define_vault(),
            // Contextual for the same reason as the rest of this run: `queue` is
            // an ordinary table name, and a store that had one before this word
            // existed keeps it.
            _ if self.eat_word("queue") => self.define_queue(),
            _ if self.eat_word("series") => self.define_series(),
            // Contextual for the same reason as the rest of this run: `view` is
            // an ordinary table name, and a store that had one before this word
            // existed keeps it.
            _ if self.eat_word("view") => self.define_view(),
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `BUCKET`, `INDEX`, `FIELD`, `ANALYZER`, `USER`, `NODE`, `REPLICA`, `KAFKA CONSUMER`, `VECTOR`, `GEO`, `VAULT`, `QUEUE` or `VIEW`",
            )),
        }
    }

    /// `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine`
    ///
    /// Both clauses are required and neither has a default. The width, because
    /// declaring it is the whole capability the word adds. The distance, for the
    /// reason the index already records: a default would silently decide which
    /// queries the store can serve, and a graph built for one distance
    /// approximates that distance and no other.
    ///
    /// They are read in a fixed order rather than in any order. Two clauses is
    /// too few to be worth an order-free reader, and a fixed order is what makes
    /// the statement read the same way in every store that has one.
    fn define_vector(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("dimension") {
            return Err(self.error_here("`DIMENSION` and how wide every vector here is"));
        }
        let dimension = self.vector_dimension()?;
        if !self.eat_word("distance") {
            return Err(self.error_here("`DISTANCE` and the distance its index is built with"));
        }
        Ok(StatementKind::DefineVector {
            name,
            dimension,
            distance: self.name()?,
            if_not_exists,
        })
    }

    /// `DEFINE QUEUE jobs TIMEOUT 30s ATTEMPTS 5 SCHEMAFULL IN work`
    ///
    /// Two clauses and two flags, and they are read differently on purpose. The
    /// **clauses** — `TIMEOUT` and `ATTEMPTS` — keep their fixed order for the
    /// reason `DEFINE VECTOR`'s two do: they are what a queue is, two is too few
    /// to be worth an order-free reader, and a fixed order is what makes the
    /// statement read the same way in every store that has one. The **flags**
    /// are order-free against each other, because they are adjectives and
    /// `DEFINE TABLE` already reads its own that way.
    ///
    /// The timeout is a literal duration rather than an expression, the rule the
    /// query timeout already keeps: a budget a bound value could set is a budget
    /// a caller could raise, and this one is meant to be readable in the
    /// statement that declared it rather than in a bindings map somewhere else.
    fn define_queue(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("timeout") {
            return Err(self.error_here("`TIMEOUT` and how long a claim holds a record"));
        }
        let expected = "a duration, like `30s` or `5m`";
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let timeout = *written;
        let at = self.span_here();
        self.advance();
        // Refused here rather than at the claim, for the reason the query
        // timeout gives about its own zero: a hold of no length is not a hold,
        // so the declaration could only ever produce a queue that hands one
        // record to every worker at once — which is a mistake in the statement,
        // and the statement is where it is worth saying so.
        if timeout.seconds() < 0 || (timeout.seconds() == 0 && timeout.nanos() == 0) {
            return Err(Error::EmptyTimeout {
                written: timeout.to_literal(),
                span: at,
            });
        }
        let attempts = if self.eat_word("attempts") {
            Some(self.attempt_ceiling()?)
        } else {
            None
        };
        // The flags come after the clauses that are the subject of the
        // statement, and they are order-free against each other — both rules
        // read off `DEFINE TABLE`, where the same comment explains why: the
        // timeout and the ceiling are what a queue *is*, the flags are
        // adjectives on it, and a grammar that insisted on an order between two
        // adjectives would only be remembered wrong.
        let mut strictness: Option<bool> = None;
        let mut graph: Option<Name> = None;
        loop {
            if strictness.is_none() && self.eat_keyword(Keyword::Schemafull) {
                strictness = Some(true);
            } else if strictness.is_none() && self.eat_keyword(Keyword::Schemaless) {
                strictness = Some(false);
            } else if graph.is_none() && self.eat_keyword(Keyword::In) {
                graph = Some(self.name()?);
            } else {
                break;
            }
        }
        Ok(StatementKind::DefineQueue {
            name,
            timeout,
            attempts,
            // Lenient unless the word says otherwise, which is `DEFINE TABLE`'s
            // own default for a declaration carrying no columns. A queue never
            // carries any, so there is no reading under which it starts strict.
            schemafull: strictness.unwrap_or(false),
            graph,
            if_not_exists,
        })
    }

    /// `DEFINE SERIES readings RETAIN 30d`
    ///
    /// The retention is a literal duration rather than an expression, on
    /// [`Self::define_queue`]'s rule and for its reason: a floor a bound value
    /// could set is a floor a caller could move, and this one is meant to be
    /// readable in the statement that declared it.
    fn define_series(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("retain") {
            return Err(self.error_here("`RETAIN` and how far back the table answers"));
        }
        let expected = "a duration, like `30d` or `12h`";
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let retain = *written;
        let at = self.span_here();
        self.advance();
        // Refused here for the reason a queue's zero timeout is: a retention of
        // no length is not a retention, it is a table that answers with nothing,
        // and that is a mistake in the statement rather than a configuration.
        if retain.seconds() < 0 || (retain.seconds() == 0 && retain.nanos() == 0) {
            return Err(Error::EmptyRetention {
                written: retain.to_literal(),
                span: at,
            });
        }
        Ok(StatementKind::DefineSeries {
            name,
            retain,
            if_not_exists,
        })
    }

    /// `DEFINE VIEW active AS SELECT * FROM users WHERE active = true`
    ///
    /// The read runs to the end of the statement. No parentheses, because there
    /// is nothing to disambiguate: a view holds exactly one `SELECT` and it is
    /// everything after `AS`.
    ///
    /// # Parsed to validate, kept as text to store
    ///
    /// The read is parsed here so that a view which is not one `SELECT` is
    /// refused where somebody wrote it rather than on whatever read first names
    /// it, and then the **source text** is what the statement carries — sliced
    /// by the read's own span. Rendering the parsed tree back would store a
    /// different statement that happens to mean the same thing, and a reader
    /// comparing what they wrote against what `INFO` reports should find them
    /// equal.
    ///
    /// Nothing is resolved: the tables the read names need not exist yet, the
    /// same rule a field's `DEFAULT` follows. The alternative would make the
    /// order of a provisioning script load-bearing.
    fn define_view(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_keyword(Keyword::As) {
            return Err(self.error_here("`AS` and the read this name means"));
        }
        if self.peek_keyword() != Some(Keyword::Select) {
            return Err(self.error_here("`SELECT` — a view is a read"));
        }
        let read = self.select_statement()?;
        Ok(StatementKind::DefineView {
            name,
            read: self.source[read.span.start..read.span.end].to_owned(),
            if_not_exists,
        })
    }

    /// The whole number after `ATTEMPTS`, which must be one and must be at least one.
    ///
    /// Zero is refused rather than read as unlimited. Unlimited already has a
    /// spelling — leaving the clause out — and a second one that looks like
    /// "never hand this out" would be the one somebody writes by accident.
    fn attempt_ceiling(&mut self) -> Result<u32> {
        let expected = "how many times a record may be handed out, like `5`";
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        // Read before advancing, so the refusal points at the number rather than
        // at whatever follows it.
        let Some(ceiling) = u32::try_from(*written).ok().filter(|held| *held > 0) else {
            return Err(self.error_here(expected));
        };
        self.advance();
        Ok(ceiling)
    }

    /// `CLAIM FROM jobs` · `CLAIM 10 FROM jobs`
    ///
    /// The count sits before `FROM` rather than in a `LIMIT` after the table,
    /// because it is not a ceiling on an answer that was going to be produced
    /// anyway — it is how much work this statement takes, and a `LIMIT` that
    /// decided how many records got written would be the one clause in the
    /// language that changes the store rather than the answer.
    fn claim_statement(&mut self, start: Span) -> Result<StatementKind> {
        // Three shapes, told apart by the token after `CLAIM` rather than by a
        // keyword: a number commits to the counting form, `FROM` is that form
        // with a count of one, and anything else is a record the caller named.
        if matches!(self.peek(), Some(Token::Number(_))) {
            let count = self.claim_count()?;
            self.expect_keyword(Keyword::From, "`FROM` and the queue to take from")?;
            let table = self.table_ref()?;
            return Ok(StatementKind::Claim {
                table,
                count,
                span: start.to(self.span_behind()),
            });
        }
        if self.eat_keyword(Keyword::From) {
            let table = self.table_ref()?;
            return Ok(StatementKind::Claim {
                table,
                count: 1,
                span: start.to(self.span_behind()),
            });
        }
        let table = self.table_ref()?;
        // Raised here rather than by `record_target_after`, whose message is
        // shared with every other record-target statement. A bare `CLAIM jobs`
        // is most likely a forgotten `FROM` rather than a forgotten identity, so
        // the message names both forms instead of only what the parser wanted
        // next — and naming them here changes no other statement's refusal.
        if !self.at_punct(Punct::Colon) {
            return Err(self.error_here(
                "`:` and the record to hold, as in `CLAIM jobs:7` — or `FROM` \
                 and the queue to take from, as in `CLAIM FROM jobs`",
            ));
        }
        let target = self.record_target_after(table)?;
        Ok(StatementKind::ClaimRecord {
            target,
            span: start.to(self.span_behind()),
        })
    }

    /// The whole number after `CLAIM`.
    ///
    /// Zero is refused here because it is a **shape** mistake — a statement that
    /// asks for no records is not a claim — while the upper bound is refused by
    /// the store rather than by the grammar. That split is the one
    /// `DEFINE VECTOR` already makes about its distance: how much a store is
    /// willing to hand out in one statement is the store's question, and a
    /// ceiling compiled into the parser would be a second place holding it.
    fn claim_count(&mut self) -> Result<u64> {
        let expected = "how many records to claim, like `10`";
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let Some(count) = u64::try_from(*written).ok().filter(|held| *held > 0) else {
            return Err(self.error_here(expected));
        };
        self.advance();
        Ok(count)
    }

    /// `DEFINE GEO places`
    ///
    /// A name and nothing else. Where `DEFINE VECTOR` requires two clauses
    /// because a store without them is not one, a geo store is complete as soon
    /// as it exists — so there is no clause to read, and adding an optional one
    /// later leaves every store written today parsing (Q-324).
    fn define_geo(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        Ok(StatementKind::DefineGeo {
            name: self.name()?,
            if_not_exists,
        })
    }

    /// `DEFINE VAULT team`
    ///
    /// A name and nothing else, for the reason `DEFINE GEO` gives: what makes a
    /// vault a vault is a key, and a key is not a clause a caller writes. The
    /// statement mints one, which is why this is the one declaration that
    /// requires the store to be unsealed — enforced where the keyring is, not
    /// here.
    fn define_vault(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        Ok(StatementKind::DefineVault {
            name: self.name()?,
            if_not_exists,
        })
    }

    /// `DEFINE NODE ROLES serving, writable ENDPOINTS 'host:9000'`
    ///
    /// Either clause, in that order, and at least one of the two. A statement
    /// naming neither is refused rather than accepted as a no-op: it can only be
    /// a half-written one, and quietly succeeding is how an operator comes to
    /// believe a node was configured.
    ///
    /// What a clause names **replaces** what was there, and a clause left out
    /// leaves its field alone. So `DEFINE NODE ENDPOINTS …` is not a silent way
    /// to drop the roles.
    ///
    /// `ROLES NONE` clears them, and needs a spelling of its own precisely
    /// because absence is taken here. `NONE` is a whole answer rather than a
    /// member of the list, and `DEFINE REPLICA` deliberately does not take it —
    /// a peer is declared rather than amended, so an absent clause already
    /// clears there. The reasoning is in the specification, § *Draining this
    /// node*, and is not restated here: two copies of one argument drift, and
    /// the document is the one a reader of the language actually opens.
    /// `DEFINE FAILOVER AWARENESS 10s COLLECTION 10s ROUND 1s CAMPAIGN 1s LEASE 30s`
    ///
    /// **In this order, and all five.** A fixed order rather than clauses in any
    /// arrangement, because these five are read together as a set — four
    /// relations hold between them — and a reader comparing two policies in a
    /// log or a report compares them line by line. Free order would make two
    /// spellings of one policy that a human eye cannot diff.
    ///
    /// Each one is required, which is the difference from [`Self::define_node`]:
    /// that statement amends a row and an absent clause leaves its field alone,
    /// while this one replaces a checked set. A statement naming three periods
    /// could only mix new values with old ones under a single version, or
    /// perform a read-modify-write nobody can see in what they typed.
    ///
    /// The relations themselves are NOT checked here. They are checked where the
    /// policy is built, by `Failover::stated`, which is the only way to make one
    /// that is not the default — so the refusal names the direction the value is
    /// wrong in, and there is exactly one place that knows those directions. A
    /// copy of them in the parser would be a second answer that drifts.
    fn define_failover(&mut self) -> Result<StatementKind> {
        let awareness = self.period("awareness")?;
        let collection = self.period("collection")?;
        let round = self.period("round")?;
        let campaign = self.period("campaign")?;
        let lease = self.period("lease")?;
        Ok(StatementKind::DefineFailover {
            awareness,
            collection,
            round,
            campaign,
            lease,
        })
    }

    /// One named period of a failover policy, refused with its own word.
    ///
    /// The clause word is in the error rather than a generic *a duration*,
    /// because five clauses in a fixed order means the operator's mistake is
    /// almost always *which one did I leave out* — and an error that cannot say
    /// leaves them counting durations.
    ///
    /// A period of no length is refused here rather than at `Failover::stated`
    /// for the reason the queue timeout gives about its own zero: it is a
    /// mistake in the statement, and the statement is where the span is.
    fn period(&mut self, clause: &'static str) -> Result<Duration> {
        // Four of the five clause words are contextual identifiers, on the
        // reasoning `NODE` and `REPLICA` are read that way. `COLLECTION` is the
        // exception because the language already reserved it for
        // `DEFINE COLLECTION`, so it arrives as a keyword and has to be eaten as
        // one.
        //
        // The clause keeps the name anyway. Calling it something else here to
        // dodge one token kind would give the same field two spellings — one in
        // the statement, one in the row and the report — which is the drift this
        // codebase has already paid for twice. The collision is only syntactic:
        // nothing but a period clause can stand at this position.
        let taken = if clause == "collection" {
            self.eat_keyword(Keyword::Collection)
        } else {
            self.eat_word(clause)
        };
        if !taken {
            return Err(self.error_here(match clause {
                "awareness" => "`AWARENESS` and how often this node refreshes what it knows",
                "collection" => "`COLLECTION` and how long a follower waits between collecting",
                "round" => "`ROUND` and how long one election round may take",
                "campaign" => "`CAMPAIGN` and how often a node checks whether to stand",
                _ => "`LEASE` and how long a granted leadership is held",
            }));
        }
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here("a duration, like `10s` or `1m`"));
        };
        let period = *written;
        let at = self.span_here();
        self.advance();
        if period.seconds() < 0 || (period.seconds() == 0 && period.nanos() == 0) {
            return Err(Error::EmptyPeriod {
                clause,
                written: period.to_literal(),
                span: at,
            });
        }
        Ok(period)
    }

    fn define_node(&mut self) -> Result<StatementKind> {
        let roles = if self.eat_word("roles") {
            if self.eat_keyword(Keyword::None) {
                Some(Vec::new())
            } else {
                let mut named = vec![self.name()?];
                while self.eat_punct(Punct::Comma) {
                    named.push(self.name()?);
                }
                Some(named)
            }
        } else {
            None
        };
        let endpoints = if self.eat_word("endpoints") {
            let (first, _) = self.text("an endpoint, as text")?;
            let mut found = vec![first];
            while self.eat_punct(Punct::Comma) {
                let (endpoint, _) = self.text("an endpoint, as text")?;
                found.push(endpoint);
            }
            Some(found)
        } else {
            None
        };
        let retain = self.retained_records()?;
        if roles.is_none() && endpoints.is_none() && retain.is_none() {
            return Err(self.error_here("`ROLES`, `ENDPOINTS` or `RETAIN` and what to set"));
        }
        Ok(StatementKind::DefineNode {
            roles,
            endpoints,
            retain,
        })
    }

    /// `DEFINE REPLICA second AT 'host:9001' NODE '<id>' ROLES serving, writable`
    ///
    /// The endpoint is text rather than a name because a host and port is not an
    /// identifier, and it is stored as written: whether it resolves is a
    /// question for whoever dials it, and refusing an unreachable address here
    /// would make the statement's success depend on the network being up at the
    /// moment it ran.
    ///
    /// `ROLES` is optional and spelled exactly as `DEFINE NODE`'s is, because it
    /// is the same field on the same membership row (ADR-0018 §2) seen from the
    /// other side — one written about a peer, one about this node. Two spellings
    /// for one set of words would be two things to keep in step.
    ///
    /// Left out, the peer is declared with no roles, and a peer with no roles
    /// takes no writes. That is the safe absence: the operator who forgot the
    /// clause gets a refusal naming it, where the opposite default would send a
    /// write to a node nobody said could take one.
    ///
    /// # `NODE`, and what saying it turns the row into
    ///
    /// `NODE` binds the row to one node by the id that node gave itself. It is
    /// optional, and without it the statement means what it has always meant.
    /// With it, the row stops being a note about somewhere else and becomes the
    /// **desired role** of a named machine: the node whose own id this is reads
    /// the row's `ROLES` as what it is supposed to be, and reconciles what it
    /// actually holds toward it the next time it opens the store.
    ///
    /// The value is written as text and is the spelling `INFO FOR NODE` prints
    /// for `id` — thirty-two hex digits — because an operator binds a node by
    /// copying that field, and a clause that would not take what the answer
    /// gives is a clause with a conversion step nobody documented. The canonical
    /// hyphenated form is taken too, since one reader already accepts both and a
    /// second reader would disagree with the first eventually.
    ///
    /// A malformed id is refused **here**, where the span is, rather than stored
    /// and puzzled over later: a row naming a node nobody will ever be is
    /// indistinguishable, afterwards, from a row nobody bound.
    fn define_replica(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("at") {
            return Err(self.error_here("`AT` and where the peer is reached"));
        }
        let (endpoint, _) = self.text("the endpoint, as text")?;
        let node = if self.eat_word("node") {
            let (written, at) = self.text("the node's id, as text")?;
            // The same refusal a `uuid` literal gets, from the same reader, so
            // the two spellings of one value cannot come to disagree about which
            // texts are ids.
            let bytes = parse_uuid(&written).ok_or(Error::InvalidUuid {
                text: written.clone(),
                span: at,
            })?;
            Some(bytes)
        } else {
            None
        };
        let roles = if self.eat_word("roles") {
            let mut named = vec![self.name()?];
            while self.eat_punct(Punct::Comma) {
                named.push(self.name()?);
            }
            Some(named)
        } else {
            None
        };
        // Read with the same reader `DEFINE USER … ON` uses, so the reach a
        // subscription names and the reach a grant names cannot come to accept
        // different spellings. `STORE`, `NAMESPACE x` and `DATABASE x.y` only —
        // the bare `x.y` that `ON` also takes is not offered here, because after
        // `REPLICATES` a bare pair would sit where a table name could and this
        // clause has no history to keep.
        let replicates = if self.eat_word("replicates") {
            if self.eat_word("shard") {
                Some(self.shard_reach()?)
            } else {
                match self.reach_keyword()? {
                    Some(reach) => Some(reach),
                    None => {
                        return Err(self.error_here(
                            "`STORE`, `NAMESPACE`, `DATABASE` or `SHARD` after `REPLICATES`",
                        ));
                    }
                }
            }
        } else {
            None
        };
        // ADR-0082. The same reader as `REPLICATES`, less `STORE`: the store is
        // what every standing node already stands for, so a placement naming it
        // would carve the whole store out of itself.
        let leads = if self.eat_word("leads") {
            if self.eat_word("shard") {
                Some(self.shard_reach()?)
            } else {
                match self.reach_keyword()? {
                    Some(ReachRef::Store) | None => {
                        return Err(self.error_here(
                            "`NAMESPACE`, `DATABASE` or `SHARD` after `LEADS` \
                             (every standing node already stands for the store)",
                        ));
                    }
                    Some(reach) => Some(reach),
                }
            }
        } else {
            None
        };
        Ok(StatementKind::DefineReplica {
            name,
            endpoint,
            roles,
            node,
            replicates,
            leads,
            if_not_exists,
        })
    }

    /// ```text
    /// DEFINE CONSUMER orders_in
    ///     FROM 'broker-1:9092', 'broker-2:9092'
    ///     TOPIC 'orders'
    ///     GROUP 'shop-orders'
    ///     FORMAT json
    ///     INTO shop.orders
    ///     IDENTITY order_id
    ///     MAP amount AS total, placed.at AS placed_at
    ///     ON FAILURE quarantine
    ///     PARALLELISM 2
    /// ```
    ///
    /// Every clause is required except `PARALLELISM`, and they are read in that
    /// order. A fixed order rather than a free one for `DEFINE NODE`'s reason in
    /// a stronger key: with nine clauses, accepting any order means the refusal
    /// for a missing one can no longer name it — the parser would only be able
    /// to say that *something* is missing, at the end, where the author has no
    /// idea which.
    ///
    /// **`on_failure` has no default.** A default here is a decision about data
    /// loss taken by whoever did not type the clause (ADR-0023 §4).
    fn define_consumer(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;

        self.expect_keyword(Keyword::From, "`FROM` and the brokers to read from")?;
        let (first, _) = self.text("a broker, as text")?;
        let mut brokers = vec![first];
        while self.eat_punct(Punct::Comma) {
            let (broker, _) = self.text("a broker, as text")?;
            brokers.push(broker);
        }
        if !self.eat_word("topic") {
            return Err(self.error_here("`TOPIC` and the topic to read"));
        }
        let (topic, _) = self.text("the topic, as text")?;

        // Declared, never derived. Deriving it from the node id would be a bug
        // that only shows up in a cluster, where every node would form its own
        // group and every node would then consume every message.
        if !self.eat_word("group") {
            return Err(self.error_here("`GROUP` and the consumer group, as text"));
        }
        let (group, _) = self.text("the consumer group, as text")?;

        if !self.eat_word("format") {
            return Err(self.error_here("`FORMAT` and how a message becomes fields"));
        }
        let format = self.name()?;

        if !self.eat_word("into") {
            return Err(self.error_here("`INTO` and the table the records land in"));
        }
        let destination = self.table_ref()?;

        // Required, and it is what makes a replayed message converge to one
        // record instead of two — so it is the clause the at-least-once claim
        // rests on rather than an optional nicety.
        if !self.eat_word("identity") {
            return Err(
                self.error_here("`IDENTITY` and the message field carrying the record's id")
            );
        }
        let identity = self.field_path()?;

        if !self.eat_word("map") {
            return Err(
                self.error_here("`MAP` and which message fields become which record fields")
            );
        }
        let mut mapping = vec![self.field_mapping()?];
        while self.eat_punct(Punct::Comma) {
            mapping.push(self.field_mapping()?);
        }

        self.expect_keyword(
            Keyword::On,
            "`ON FAILURE` and what to do with a message that cannot be applied",
        )?;
        if !self.eat_word("failure") {
            return Err(self.error_here("`FAILURE`, which follows `ON` here"));
        }
        let on_failure = if self.eat_word("stop") {
            OnFailure::Stop
        } else if self.eat_word("quarantine") {
            OnFailure::Quarantine
        } else {
            // Both named, and no third offered. A skip mode is the one this
            // grammar deliberately does not have.
            return Err(self.error_here("`STOP` or `QUARANTINE`"));
        };

        let parallelism = if self.eat_word("parallelism") {
            Some(self.whole_number("how many consumers to run")?)
        } else {
            None
        };

        Ok(StatementKind::DefineConsumer {
            name,
            source: ConsumerSource { brokers, topic },
            group,
            format,
            identity,
            mapping,
            destination,
            on_failure,
            parallelism,
            if_not_exists,
        })
    }

    /// `amount AS total` — one message field and what the record calls it.
    fn field_mapping(&mut self) -> Result<FieldMapping> {
        let from = self.field_path()?;
        self.expect_keyword(Keyword::As, "`AS` and what the record calls the field")?;
        Ok(FieldMapping {
            from,
            to: self.name()?,
        })
    }

    /// A count standing where one is required.
    ///
    /// Refused rather than clamped when it does not fit or is not positive: a
    /// clamped count is a statement that ran as something other than what it
    /// says, which is the class of bug this grammar spends refusals to avoid.
    fn whole_number(&mut self, expected: &'static str) -> Result<u32> {
        let Some(Token::Number(tessari_types::Number::Integer(held))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let held = u32::try_from(*held).map_err(|_| self.error_here(expected))?;
        if held == 0 {
            return Err(self.error_here(expected));
        }
        self.advance();
        Ok(held)
    }

    /// `RETAIN 100000 RECORDS` or `RETAIN NONE` after `DEFINE NODE`, when it is
    /// there.
    ///
    /// The outer `Option` is *was the clause written*, and the inner one is
    /// *what it said*, which is the shape every amending clause on this
    /// statement has: absent means leave the setting alone, and `NONE` means put
    /// it back to unbounded.
    ///
    /// # Why `RECORDS` is required and why there is no other unit
    ///
    /// A bare number would leave the reader to guess between records, bytes and
    /// a duration, and the three have different failure modes — only one of them
    /// is a count this store can enforce exactly, because a log position IS a
    /// record count. Bytes and ages are both derived quantities here and would
    /// have to be approximated; a clause that says `RECORDS` cannot be silently
    /// re-read as either.
    ///
    /// # Why zero is refused rather than clamped
    ///
    /// `RETAIN 0 RECORDS` reads as *keep nothing*, and keeping nothing is the
    /// one setting that must not be expressible: a level follower is served by
    /// reading the record BEFORE the position it asks for, so a log with no
    /// records left cannot answer a follower that is perfectly healthy. The
    /// store clamps anyway — the last record always survives — but a statement
    /// that runs as something other than what it says is the class of bug this
    /// grammar spends refusals to avoid.
    fn retained_records(&mut self) -> Result<Option<Option<u64>>> {
        if !self.eat_word("retain") {
            return Ok(None);
        }
        if self.eat_keyword(Keyword::None) {
            return Ok(Some(None));
        }
        let expected = "`RETAIN n RECORDS`, or `RETAIN NONE` to keep the whole log";
        let Some(Token::Number(Number::Integer(held))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let held = u64::try_from(*held).unwrap_or(0);
        if held == 0 {
            return Err(self.error_here(expected));
        }
        self.advance();
        if !self.eat_word("records") {
            return Err(self.error_here(expected));
        }
        Ok(Some(Some(held)))
    }

    /// `MAX 5242880` after a bucket's name, when it is there.
    ///
    /// A count of **bytes**, written out. `5MB` is not a spelling this grammar
    /// has: digits touching a letter are a duration whatever the letter is
    /// (see the lexer), so `5MB` would be a duration with an unrecognised unit
    /// and refused. Giving the clause a shorter spelling means changing that
    /// rule for every literal in the language, which is a large change bought
    /// for a small convenience.
    ///
    /// A zero refuses rather than clamping, for the reason `DEPTH 0` does: a
    /// bucket that accepts no file is not a bucket with a ceiling, it is a
    /// table nothing can be written to, and a caller who wrote `MAX 0` meant
    /// something else.
    fn byte_ceiling(&mut self) -> Result<Option<u64>> {
        if !self.eat_word("max") {
            return Ok(None);
        }
        let expected = "`MAX n` — the largest file the bucket takes, in bytes";
        let Some(Token::Number(Number::Integer(held))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        // A negative and a zero refuse as the same thing, which they are: both
        // say the bucket admits no file at all.
        let held = u64::try_from(*held).unwrap_or(0);
        if held == 0 {
            return Err(self.error_here(expected));
        }
        self.advance();
        Ok(Some(held))
    }

    /// `FROM users TO users ORDER BY at DESC` after `EDGE`, when it is there.
    ///
    /// The pair is read as a unit: `FROM` without `TO` is refused rather than
    /// read as half a declaration, because an edge table that knows only where
    /// its edges leave from could refuse nothing a permissive one accepts, and
    /// the statement would have bought its clause for nothing.
    fn edge_clause(&mut self) -> Result<EdgeClause> {
        if !self.eat_keyword(Keyword::From) {
            return Ok(EdgeClause::Any);
        }
        let from = self.table_ref()?;
        self.expect_keyword(Keyword::To, "`TO` and the table an edge leads into")?;
        let to = self.table_ref()?;
        let order = self.edge_ordering()?;
        Ok(EdgeClause::Between(Box::new(EdgeEndpoints {
            from,
            to,
            order,
        })))
    }

    /// `ORDER BY at DESC` after an edge table's declared pair, when it is there.
    ///
    /// Read with contextual words for the reason `shape.rs` reads the same
    /// clause that way: reserving `ORDER` would take a good column name out of
    /// every table in the store to buy nothing, since only a clause word can
    /// stand in this position.
    ///
    /// The key is a single field **name**, not the routed expression a `SELECT`
    /// orders by. It becomes the endpoint index's key suffix, so it has to be
    /// something the writer can read off the edge as it places it.
    fn edge_ordering(&mut self) -> Result<Option<EdgeOrdering>> {
        if !self.eat_word("order") {
            return Ok(None);
        }
        if !self.eat_word("by") {
            return Err(self.error_here("`BY` after `ORDER`"));
        }
        let field = self.name()?;
        // `ASC` is accepted and means nothing, exactly as it does in a `SELECT`:
        // a reader who writes the default is saying what they mean.
        let descending = if self.eat_word("desc") {
            true
        } else {
            self.eat_word("asc");
            false
        };
        Ok(Some(EdgeOrdering { field, descending }))
    }

    /// `DEFINE INDEX by_email ON users FIELDS email, name UNIQUE`
    fn define_index(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the index reads")?;
        let table = self.table_ref()?;
        self.expect_keyword(Keyword::Fields, "`FIELDS` and the fields to index")?;

        let mut fields = vec![self.field_path()?];
        while self.eat_punct(Punct::Comma) {
            fields.push(self.field_path()?);
        }
        // **One** marker. `UNIQUE` says how entries collide, `SEARCH` says the
        // entries are terms, `VECTOR` says they are a graph, `SPATIAL` says they
        // are cells — four different index kinds wearing four flags, of
        // which at most one can be true. Accepting two used to be possible and
        // the first one checked simply won, so `UNIQUE SEARCH` was an index
        // whose uniqueness was silently ignored. Adding a third made that
        // inconsistency a thing to answer rather than inherit.
        /// Which of the four an index is.
        enum Marker {
            Unique,
            Search,
            Spatial,
            /// With the distance its graph is built for, which is required —
            /// a default would silently decide which queries the index serves.
            Vector(crate::Name),
        }
        let mut kind: Option<Marker> = None;
        loop {
            let held = if self.eat_keyword(Keyword::Unique) {
                Marker::Unique
            } else if self.eat_keyword(Keyword::Search) {
                Marker::Search
            } else if self.eat_word("spatial") {
                // Contextual for the same reason `vector` is: a field called
                // `spatial` is not a name to take away from a caller.
                Marker::Spatial
            } else if self.eat_word("vector") {
                // Contextual, like `order` and `fetch`: a field called `vector`
                // in a database of embeddings is not a name to take away.
                Marker::Vector(self.name()?)
            } else {
                break;
            };
            if kind.is_some() {
                return Err(self.error_here("one index kind, not two"));
            }
            kind = Some(held);
        }
        // An index over `tags[*]` is a **multikey** index: one entry per element
        // rather than one per record. Three shapes are refused, each naming its
        // own reason — a caller told "unexpected token" would go looking for a
        // typo in a statement that has none.
        let mut several = fields.iter().filter(|field| field.path.is_several());
        if let Some(field) = several.next() {
            match &kind {
                Some(Marker::Unique) => {
                    return Err(Error::SeveralInAUniqueIndex { span: field.span });
                }
                Some(Marker::Search | Marker::Vector(_) | Marker::Spatial) => {
                    return Err(Error::SeveralInAnAnalysedIndex { span: field.span });
                }
                None => {}
            }
            if let Some(second) = several.next() {
                return Err(Error::SeveralRoutesInOneIndex { span: second.span });
            }
        }
        Ok(StatementKind::DefineIndex {
            name,
            table,
            fields,
            unique: matches!(kind, Some(Marker::Unique)),
            search: matches!(kind, Some(Marker::Search)),
            spatial: matches!(kind, Some(Marker::Spatial)),
            vector: match kind {
                Some(Marker::Vector(distance)) => Some(distance),
                _ => None,
            },
            if_not_exists,
        })
    }

    /// `DEFINE USER ada ON prod.orders ROLE editor PASSWORD '…'`
    ///
    /// `ON` names a tenancy the way `orders.users` names a table; without it the
    /// user belongs to the store and is its root.
    fn define_user(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let scope = if self.eat_keyword(Keyword::On) {
            Some(self.reach_ref()?)
        } else {
            None
        };
        // `ROLE` first because it is what every existing statement says, and
        // `AUTHORITIES` reached only when the statement does not say `ROLE` —
        // so no spelling that parses today parses differently now.
        let role = if self.eat_keyword(Keyword::Role) {
            UserGrant::Role(self.name()?)
        } else if self.eat_word("authorities") {
            UserGrant::Authorities(self.kind_list()?)
        } else {
            return Err(self.error_here("`ROLE` or `AUTHORITIES` and what the user may do"));
        };
        self.expect_keyword(Keyword::Password, "`PASSWORD` and the credential")?;
        let (password, _) = self.text("the password, as text")?;
        Ok(StatementKind::DefineUser {
            name,
            scope,
            role,
            password: Password::new(password),
            if_not_exists,
        })
    }

    /// `ALTER USER ada SET PASSWORD '…'` · `ALTER TABLE users SET SCHEMAFULL`
    ///
    /// The target is spelled out, and the comment this replaces predicted why:
    /// `ALTER ada SET …` reads as though there were one namespace of alterable
    /// things, and a table becoming alterable is exactly the case that would
    /// have made that reading wrong everywhere it was already written.
    fn alter_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        if self.eat_keyword(Keyword::Table) {
            let table = self.table_ref()?;
            // The columnar spellings first, because `SET` is the one that reads
            // as a whole-table change and the three field verbs read as changes
            // to something inside it.
            if self.eat_word("add") {
                self.expect_keyword(Keyword::Field, "`FIELD` and the field to add")?;
                let name = self.declared_field_name()?;
                return self.field_declaration(name, table, false);
            }
            if self.eat_keyword(Keyword::Alter) {
                self.expect_keyword(Keyword::Field, "`FIELD` and the field to change")?;
                let name = self.declared_field_name()?;
                return self.field_declaration(name, table, true);
            }
            if self.eat_keyword(Keyword::Drop) {
                self.expect_keyword(Keyword::Field, "`FIELD` and the field to remove")?;
                return Ok(StatementKind::DropField {
                    name: self.declared_field_name()?,
                    table,
                });
            }
            self.expect_keyword(
                Keyword::Set,
                "`SET`, `ADD FIELD`, `ALTER FIELD` or `DROP FIELD`",
            )?;
            let change = if self.eat_keyword(Keyword::Schemafull) {
                TableChange::Schemafull
            } else if self.eat_keyword(Keyword::Schemaless) {
                TableChange::Schemaless
            } else {
                return Err(self.error_here("`SCHEMAFULL` or `SCHEMALESS`"));
            };
            return Ok(StatementKind::AlterTable { table, change });
        }
        if self.eat_keyword(Keyword::Namespace) {
            let name = self.name()?;
            let Some(replication) = self.replication_clause()? else {
                return Err(self.error_here("`REPLICATION` and the policy to set"));
            };
            return Ok(StatementKind::AlterNamespace { name, replication });
        }
        if !self.eat_keyword(Keyword::User) {
            return Err(self.error_here("`NAMESPACE`, `USER` or `TABLE` and the thing to change"));
        }
        let name = self.name()?;
        self.expect_keyword(Keyword::Set, "`SET` and the one thing to change")?;
        let change = match self.peek_keyword() {
            Some(Keyword::Password) => {
                self.advance();
                let (password, _) = self.text("the new password, as text")?;
                UserChange::Password(Password::new(password))
            }
            Some(Keyword::Role) => {
                self.advance();
                UserChange::Role(self.name()?)
            }
            _ => return Err(self.error_here("`PASSWORD` or `ROLE`")),
        };
        Ok(StatementKind::AlterUser { name, change })
    }

    /// `DEFINE ANALYZER simple FILTERS lowercase, ascii`
    ///
    /// The tokenizer is not named because there is one: splitting on
    /// non-alphanumeric boundaries is what every filter chain assumes
    /// underneath it, and a knob with one setting is a knob nobody should have
    /// to read about.
    fn define_analyzer(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let mut filters = Vec::new();
        if self.eat_keyword(Keyword::Filters) {
            filters.push(self.filter()?);
            while self.eat_punct(Punct::Comma) {
                filters.push(self.filter()?);
            }
        }
        Ok(StatementKind::DefineAnalyzer {
            name,
            filters,
            if_not_exists,
        })
    }

    /// One named filter.
    fn filter(&mut self) -> Result<Filter> {
        let Some(Token::Ident(word)) = self.peek() else {
            return Err(self.error_here("a filter name"));
        };
        let Some(filter) = Filter::parse(word) else {
            return Err(self.error_here("a filter name"));
        };
        self.advance();
        Ok(filter)
    }

    /// The name of a field being declared, altered or dropped.
    ///
    /// A field name in an object literal already reads with `field_name()`,
    /// which takes a quoted name — so `{ 'password': '…' }` writes a field that
    /// `DEFINE FIELD password …` could not declare, because `PASSWORD` is a
    /// keyword and `name()` does not accept one. The write worked and the
    /// declaration did not, and quoting did not help either: the two halves of
    /// the language disagreed about what a field may be called.
    ///
    /// This accepts the quoted form here too, and **only** the quoted form. A
    /// bare keyword is still refused, which keeps the wider question — whether
    /// `DEFINE FIELD password` should read as a name — open and separate
    /// (Q-417). The narrow version is worth having on its own because a string
    /// literal in a name position cannot be anything else: no existing statement
    /// changes meaning, only refusals become parses, and a statement that forgot
    /// its name is still missing a name, because a missing name is not a string.
    ///
    /// It matters most in a vault, which is strict, so a field nobody can
    /// declare is a field a vault cannot hold — and `password` is the first
    /// thing a secret store will be asked for.
    fn declared_field_name(&mut self) -> Result<Name> {
        if matches!(self.peek(), Some(Token::Str(_))) {
            let (text, span) = self.quoted_field_name()?;
            return Ok(Name { text, span });
        }
        self.name()
    }

    /// `DEFINE FIELD email ON users TYPE string`
    fn define_field(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.declared_field_name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the field is on")?;
        let table = self.table_ref()?;
        self.declaration_tail(name, table, if_not_exists, false)
    }

    /// The half of a field declaration that follows its name and its table.
    ///
    /// Shared with `ALTER TABLE … ADD FIELD` and `… ALTER FIELD`, which name the
    /// same two things in the other order and then say exactly the same thing
    /// about the field. Written once so the two spellings cannot drift — a
    /// second copy is how `DEFAULT` ends up accepted by one of them and not the
    /// other.
    fn field_declaration(
        &mut self,
        name: Name,
        table: TableRef,
        replacing: bool,
    ) -> Result<StatementKind> {
        self.declaration_tail(name, table, false, replacing)
    }

    fn declaration_tail(
        &mut self,
        name: Name,
        table: TableRef,
        if_not_exists: bool,
        replacing: bool,
    ) -> Result<StatementKind> {
        self.expect_keyword(Keyword::Type, "`TYPE` and what the field may hold")?;
        let kind = self.field_kind()?;
        let marker = self.span_here();
        let FieldOptions {
            required,
            secret,
            default,
            analyzer,
            assert,
        } = self.field_options()?;
        if replacing {
            if secret {
                // There is no `ALTER FIELD … SECRET`. Turning the marker on
                // leaves every record already written in the clear; turning it
                // off leaves every record already written unreadable. Both are a
                // field that is half sealed, and a statement that produced
                // either would report success.
                return Err(Error::Unsupported {
                    feature: "altering a field to or from `SECRET` — declare it \
                              `SECRET` when the vault's field is defined",
                    span: marker,
                });
            }
            return Ok(StatementKind::AlterField {
                name,
                table,
                kind,
                required,
                default,
                analyzer,
                assert,
            });
        }
        Ok(StatementKind::DefineField {
            name,
            table,
            kind,
            required,
            secret,
            default,
            analyzer,
            assert,
            if_not_exists,
        })
    }

    /// Everything a field declaration says after its type.
    ///
    /// Shared by all three spellings — `DEFINE FIELD`, `ALTER TABLE … FIELD`,
    /// and a column inside `DEFINE TABLE`'s parentheses — because a reader who
    /// learns `DEFAULT` in one of them has learned it in the others, and three
    /// copies of this loop is how one of them quietly stops accepting `ASSERT`.
    fn field_options(&mut self) -> Result<FieldOptions> {
        // Any marker, in any order, and none twice — the rule `DEFINE TABLE`'s
        // two flags already follow, for the same reason: there is no reading
        // under which one has to precede the other, and a grammar that insisted
        // would only be remembered wrong.
        let mut options = FieldOptions::default();
        loop {
            if !options.required && self.eat_keyword(Keyword::Required) {
                options.required = true;
            } else if options.default.is_none() && self.eat_keyword(Keyword::Default) {
                options.default = Some(self.written_expression()?);
            } else if !options.secret && self.eat_word("secret") {
                // Contextual, like `assert` and `vector` above. `secret` is an
                // ordinary column name in plenty of schemas and reserving it
                // here would reserve it in every position, including as the name
                // of the very field somebody is trying to declare.
                options.secret = true;
            } else if options.analyzer.is_none() && self.eat_keyword(Keyword::Analyzer) {
                options.analyzer = Some(self.name()?);
            } else if options.assert.is_none() && self.eat_word("assert") {
                // Contextual, like `vector` and `fetch`: nothing but this marker
                // can stand here, and a field called `assert` is not a name to
                // take away from a table that has one.
                options.assert = Some(super::assertion::lower(&self.condition()?)?);
            } else {
                break;
            }
        }
        Ok(options)
    }

    /// The parenthesised column list of a columnar `DEFINE TABLE`, if it has one.
    ///
    /// Empty parentheses are refused rather than read as no columns: the
    /// flag-only spelling already says *no columns* by writing nothing, so `()`
    /// can only be a list somebody meant to fill in.
    fn columns(&mut self) -> Result<Vec<ColumnDeclaration>> {
        if !self.eat_punct(Punct::ParenOpen) {
            return Ok(Vec::new());
        }
        let mut columns = Vec::new();
        loop {
            let name = self.name()?;
            let kind = self.field_kind()?;
            let marker = self.span_here();
            let FieldOptions {
                required,
                secret,
                default,
                analyzer,
                assert,
            } = self.field_options()?;
            if secret {
                // A columnar `DEFINE TABLE` is not a vault and cannot become
                // one, so a `SECRET` here has nowhere to be honoured. Refused
                // rather than parsed and dropped: a marker silently ignored is
                // the failure this whole feature exists to prevent.
                return Err(Error::Unsupported {
                    feature: "`SECRET` in a columnar table declaration — a \
                              secret field belongs on a `DEFINE VAULT`",
                    span: marker,
                });
            }
            columns.push(ColumnDeclaration {
                name,
                kind,
                required,
                default,
                analyzer,
                assert,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::ParenClose, "`)` closing the column list")?;
        Ok(columns)
    }

    /// A type name, which may be spelled with a reserved word.
    ///
    /// `table`, `set`, `range`, `datetime` and `uuid` are all reserved
    /// elsewhere, and after `TYPE` nothing but a type name can appear — so the
    /// word is read as text here, the way a field name inside an object literal
    /// already is. The alternative is a language where five of the seventeen
    /// types cannot be written down.
    fn field_kind(&mut self) -> Result<FieldKind> {
        // A union is spelled by its members, so it is recognised by one of them
        // standing where a type name would. Nothing else in a declaration puts
        // a string here, so the two readings cannot collide.
        if matches!(self.peek(), Some(Token::Str(_))) {
            return self.literal_union();
        }
        // Read from the tokens rather than through `FieldKind::parse`, which
        // takes a single spelling: a width is four tokens. Contextual like
        // `order` and `fetch`, and for the reason `DEFINE INDEX … VECTOR`
        // already gives — a field called `vector` in a database of embeddings is
        // not a name to take away. It costs nothing here, because the name is
        // read before the type in both declarations that reach this.
        if self.eat_word("vector") {
            return self.vector_width();
        }
        let spelling = match self.peek() {
            Some(Token::Keyword(keyword)) => keyword.spelling().to_owned(),
            Some(Token::Ident(name)) => name.clone(),
            _ => return Err(self.error_here("a type name")),
        };
        let Some(kind) = FieldKind::parse(&spelling) else {
            return Err(self.error_here("a type name"));
        };
        self.advance();
        Ok(kind)
    }

    /// `<768>` — how many numbers every vector in this field holds.
    ///
    /// The width is required, and there is no width-less `vector`. A vector
    /// whose length is not declared is an `array`, which the language already
    /// has: the word would say something about the author's intention and
    /// nothing that could be checked, and a field that looks checked and is not
    /// is worse than one that never claimed to be.
    ///
    /// A **literal**, like `DEPTH n` and for a related reason. A width read from
    /// a parameter would be a schema whose shape depends on what was bound at
    /// the moment the declaration ran, and the catalog has to store one answer.
    /// `APPROXIMATE`, and the budget it may carry.
    ///
    /// `EFFORT` stands only after `APPROXIMATE` — never on its own and never
    /// before it — because a budget without the permission is a number with
    /// nothing to spend it on: an exact scan visits every record by definition.
    /// So the pair is read here as one thing and stored as one value, and the
    /// illegal half is not expressible.
    ///
    /// Both words are contextual, like the rest of this tail: a field called
    /// `approximate` or `effort` stays a field.
    fn approximation(&mut self) -> Result<Option<Approximation>> {
        if !self.eat_word("approximate") {
            return Ok(None);
        }
        if !self.eat_word("effort") {
            return Ok(Some(Approximation::Default));
        }
        let span = self.span_here();
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here("a whole number of candidates, written out"));
        };
        // A negative and a zero refuse as the same thing, as they do for a width
        // and for a depth: both say fewer than one candidate, and a walk that may
        // keep none is a search with no way to answer.
        let candidates = usize::try_from(*written).unwrap_or(0);
        self.advance();
        if candidates == 0 {
            return Err(Error::EffortBelowOne { span });
        }
        Ok(Some(Approximation::Effort(candidates)))
    }

    /// `WITHOUT SCAN GUARD`, which lifts the planner's veto for this read.
    ///
    /// Three words rather than one, and that is the point. The clause changes
    /// which plan runs, so a reader skimming the tail must not be able to take
    /// it for decoration — `USING INDEX` was rejected as the place to put it for
    /// the same reason, since a modifier that turns an assertion into an
    /// instruction is a pun.
    ///
    /// All three words are contextual, like the rest of this tail: a field, a
    /// table or an index called `without`, `scan` or `guard` stays itself.
    /// `WITHOUT` only begins this clause where a clause may begin, and once it
    /// has, the two words after it are required — a bare `WITHOUT` names nothing
    /// this planner has, and guessing at what was meant would be inventing a
    /// second spelling nobody documented.
    fn scan_guard(&mut self) -> Result<bool> {
        if !self.eat_word("without") {
            return Ok(false);
        }
        self.expect_word("scan", "`SCAN GUARD` — the guard `WITHOUT` lifts")?;
        self.expect_word("guard", "`GUARD`, completing `WITHOUT SCAN GUARD`")?;
        Ok(true)
    }

    fn vector_width(&mut self) -> Result<FieldKind> {
        self.expect_punct(Punct::Less, "`<` and the width every vector here holds")?;
        let span = self.span_here();
        let width = self.vector_dimension()?;
        self.expect_punct(Punct::Greater, "`>` closing the width")?;
        FieldKind::vector(width).ok_or(Error::VectorWidthBelowOne { span })
    }

    /// The whole number of components a declaration names.
    ///
    /// Shared by the two places a width is written — `TYPE vector<n>` on a field
    /// and `DIMENSION n` on a store — so that the language has one answer to
    /// *which numbers are widths* rather than one answer per doorway. That is
    /// the same reason `field_kind` is called by both field spellings.
    fn vector_dimension(&mut self) -> Result<usize> {
        let span = self.span_here();
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here("a whole number of components, written out"));
        };
        // A negative and a zero refuse as the same thing, which they are: both
        // say fewer than one component, and a declaration that can hold only the
        // empty array is one no useful write satisfies.
        let written = usize::try_from(*written).unwrap_or(0);
        self.advance();
        if written > crate::WIDEST_VECTOR {
            return Err(Error::VectorWidthAboveTheCeiling {
                most: crate::WIDEST_VECTOR,
                span,
            });
        }
        // Asked of the type rather than tested here, so the rule lives where the
        // kind that carries it lives and cannot drift from it.
        match FieldKind::vector(written) {
            Some(FieldKind::Vector(width)) => Ok(width),
            _ => Err(Error::VectorWidthBelowOne { span }),
        }
    }

    /// `'draft' | 'published'` — a field that holds one of a fixed set of strings.
    ///
    /// The declaration a status column has always wanted. `TYPE string` is true
    /// and says nothing; an `ASSERT` says the same thing but says it where a
    /// reader of the schema does not look, and where a reader of an error
    /// message gets a condition rather than a list.
    ///
    /// Members are sorted and deduplicated by the constructor, so two
    /// declarations naming the same set are the same type however they were
    /// typed. A set that remembers the order somebody wrote it in is two values
    /// for one fact — the rule a grant's verbs already follow.
    fn literal_union(&mut self) -> Result<FieldKind> {
        let mut members = Vec::new();
        loop {
            let Some(Token::Str(member)) = self.peek() else {
                return Err(self.error_here("a quoted member of the union"));
            };
            members.push(member.clone());
            self.advance();
            if !self.eat_punct(Punct::Pipe) {
                break;
            }
        }
        // Unreachable while the loop pushes before it can break, and named
        // rather than unwrapped because `union` refusing an empty set is a rule
        // about the type and not about this parser.
        FieldKind::union(members).ok_or_else(|| self.error_here("a member of the union"))
    }

    /// `REBUILD INDEX <name> ON <table>`
    ///
    /// `INDEX` is spelled out although nothing else can be rebuilt yet, because
    /// the alternative reads as though the table were the thing being rebuilt.
    fn rebuild_statement(&mut self) -> Result<StatementKind> {
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
    fn check_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        self.expect_keyword(Keyword::Table, "`TABLE` and the table to check")?;
        Ok(StatementKind::CheckTable {
            table: self.table_ref()?,
        })
    }

    fn drop_statement(&mut self) -> Result<StatementKind> {
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
    fn write_statement(&mut self, verb: Keyword) -> Result<StatementKind> {
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
            Keyword::Set => StatementKind::Set { target, value },
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
    fn split_points(&mut self) -> Result<Vec<RecordId>> {
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
    fn identity_kind(&mut self) -> Result<IdentityKind> {
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
    fn insert_statement(&mut self) -> Result<StatementKind> {
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
    fn answer(&mut self, verb: Keyword) -> Result<Answer> {
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
    fn changed(
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
    fn edit_condition(&mut self, verb: Keyword) -> Result<Option<Expr>> {
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
        super::shape::no_fold(&condition)?;
        super::shape::check_several(&condition)?;
        Ok(Some(condition))
    }

    /// `name = 'grace'` — one route and what it becomes.
    fn assignment(&mut self) -> Result<Assignment> {
        let route = self.field_path()?;
        // A route reaching several values would have to say which of them
        // changes, and `[*]`'s three contexts do not include this one.
        super::shape::no_several_path(&route)?;
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

    /// `GRANT read, write ON orders TO ada` and its opposite.
    ///
    /// One function for both because they differ in two tokens and nothing else,
    /// and two nearly identical parsers is two places for the grammar to drift.
    /// `TO` and `FROM` rather than one word for both, because a reader should be
    /// able to tell which direction a statement goes without reading its verb
    /// twice.
    fn grant_statement(&mut self, giving: bool) -> Result<StatementKind> {
        self.advance();
        let mut verbs = vec![self.word_or_name()?];
        while self.eat_punct(Punct::Comma) {
            verbs.push(self.word_or_name()?);
        }
        self.expect_keyword(Keyword::On, "`ON` and the table or reach")?;
        // A reach is keyword-led in all three spellings and a table name can
        // never be a keyword, so this decision is made by the grammar rather
        // than by looking anything up. That is the whole reason `STORE` was
        // reserved: deciding it any other way would silently widen a table
        // grant somebody already wrote.
        if let Some(reach) = self.reach_keyword()? {
            let kinds = verbs;
            if giving {
                self.expect_keyword(Keyword::To, "`TO` and the user")?;
                return Ok(StatementKind::GrantAuthority {
                    kinds,
                    reach,
                    user: self.name()?,
                });
            }
            self.expect_keyword(Keyword::From, "`FROM` and the user")?;
            return Ok(StatementKind::RevokeAuthority {
                kinds,
                reach,
                user: self.name()?,
            });
        }
        let table = self.table_ref()?;
        if giving {
            // `FIELDS` narrows what may be *read*. It sits where the same word
            // sits in `DEFINE INDEX … FIELDS`, because it names the same thing.
            let mut fields = Vec::new();
            if self.eat_keyword(Keyword::Fields) {
                fields.push(self.name()?);
                while self.eat_punct(Punct::Comma) {
                    fields.push(self.name()?);
                }
            }
            self.expect_keyword(Keyword::To, "`TO` and the user")?;
            return Ok(StatementKind::Grant {
                verbs,
                table,
                fields,
                user: self.name()?,
            });
        }
        self.expect_keyword(Keyword::From, "`FROM` and the user")?;
        Ok(StatementKind::Revoke {
            verbs,
            table,
            user: self.name()?,
        })
    }

    /// A reach, when the next token opens one, and nothing consumed when not.
    ///
    /// Returning `None` rather than erroring is what lets one `ON` serve both
    /// the table grant and the authority grant: the caller falls through to a
    /// table reference having consumed nothing.
    fn reach_keyword(&mut self) -> Result<Option<ReachRef>> {
        match self.peek_keyword() {
            Some(Keyword::Store) => {
                self.advance();
                Ok(Some(ReachRef::Store))
            }
            Some(Keyword::Namespace) => {
                self.advance();
                Ok(Some(ReachRef::Namespace(self.name()?)))
            }
            Some(Keyword::Database) => {
                self.advance();
                Ok(Some(ReachRef::Database(self.table_ref()?)))
            }
            _ => Ok(None),
        }
    }

    /// `prod.shop.orders 2`, once `SHARD` has been read (G031).
    ///
    /// Fully qualified and never resolved against the session's `USE`: a
    /// subscription is a statement about the cluster, and a shard named relative
    /// to whatever the writing session happened to select would mean different
    /// shards in two scripts that read the same.
    fn shard_reach(&mut self) -> Result<ReachRef> {
        let namespace = self.name()?;
        self.expect_punct(Punct::Dot, "`.` and the database")?;
        let database = self.name()?;
        self.expect_punct(Punct::Dot, "`.` and the split table")?;
        let table = self.name()?;
        let expected = "the shard's number, as `INFO FOR TABLE` reports it";
        let Some(Token::Number(Number::Integer(shard))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let shard = u32::try_from(*shard)
            .ok()
            .filter(|shard| *shard > 0)
            .ok_or_else(|| self.error_here(expected))?;
        self.advance();
        Ok(ReachRef::Shard {
            namespace,
            database,
            table,
            shard,
        })
    }

    /// A reach in any of its spellings, including the bare `prod.orders`.
    ///
    /// The bare form is a **database** and has been since `DEFINE USER … ON
    /// prod.orders` existed. It is kept rather than deprecated because every
    /// statement already written says it.
    fn reach_ref(&mut self) -> Result<ReachRef> {
        match self.reach_keyword()? {
            Some(reach) => Ok(reach),
            None => Ok(ReachRef::Database(self.table_ref()?)),
        }
    }

    /// `manage, read` — one or more authority kinds, as written.
    ///
    /// Words rather than names because every kind is a bare word, and the
    /// kind is checked where the store knows the set rather than here, so a
    /// misspelling is one error at one place.
    fn kind_list(&mut self) -> Result<Vec<Name>> {
        let mut kinds = vec![self.word_or_name()?];
        while self.eat_punct(Punct::Comma) {
            kinds.push(self.word_or_name()?);
        }
        Ok(kinds)
    }

    /// `..` or `..=`, when a span's bound follows.
    ///
    /// Answers whether the upper bound is **inclusive**, so the two spellings
    /// are read once here rather than compared again at every use.
    fn range_bound(&mut self) -> Option<bool> {
        if self.eat_punct(Punct::DotDotEquals) {
            return Some(true);
        }
        if self.eat_punct(Punct::DotDot) {
            return Some(false);
        }
        None
    }

    /// What the `FROM` names, resolved to exactly one access path.
    fn select_source(&mut self) -> Result<Source> {
        // `$node` before the table, because it is the one source that is not a
        // name. A parameter is not legal in this position at all — a value
        // cannot say which table to read — so recognising this one takes nothing
        // away from a caller, and a parameter they *supply* called `node` stays
        // theirs, unshadowed, everywhere a value belongs. That is the difference
        // between a source spelled with a sigil and a reserved parameter name,
        // which ADR-0018's amendment rejects for exactly the shadowing it would
        // have introduced.
        if matches!(self.peek(), Some(Token::Parameter(name)) if name == NODE_SOURCE) {
            self.advance();
            return Ok(Source::Node);
        }
        if self.peek() == Some(&Token::Punct(Punct::ParenOpen)) {
            return self.subquery_source();
        }
        let table = self.table_ref()?;
        if self.peek() == Some(&Token::Punct(Punct::Colon)) {
            let record = self.record_target_after(table)?;
            // `events:1000..2000` reads as a span, and `events:1000` as one
            // record, decided by the two dots and nothing else. The identity is
            // parsed first either way, so a span costs no lookahead and a
            // record's grammar does not change.
            if let Some(inclusive) = self.range_bound() {
                // The table is not written again. `events:1000..events:2000`
                // would let somebody name two tables in one span, and there is
                // no answer to that question — so the upper bound is an
                // identity and the table is the one the lower bound named.
                let at = self.span_here();
                let upper = self.record_id(at)?;
                return Ok(Source::Range {
                    span: record.span.to(self.span_behind()),
                    table: record.table,
                    lower: record.id,
                    upper,
                    inclusive,
                });
            }
            return Ok(match self.arrow() {
                Some(direction) => self.traversal(record, direction)?,
                None => Source::Record(record),
            });
        }
        let alias = self.alias()?;
        if self.eat_keyword(Keyword::Join) {
            return self.join(JoinSide::Table { table, alias });
        }
        if alias.is_some() {
            return Err(self.aliased_without_a_join());
        }
        if self.eat_keyword(Keyword::Where) {
            return Ok(Source::Where {
                table,
                condition: Box::new(self.condition()?),
            });
        }
        Ok(Source::Table(table))
    }

    /// `AS <name>`, consumed if it is there.
    ///
    /// A failure here is returned rather than swallowed as "no alias": `AS 3`
    /// would otherwise fall through and be reported many tokens later, pointing
    /// at whatever the parser tripped over next instead of at the name.
    fn alias(&mut self) -> Result<Option<Name>> {
        if !self.eat_keyword(Keyword::As) {
            return Ok(None);
        }
        Ok(Some(self.name()?))
    }

    /// A name was given to a source that is not a side of anything.
    ///
    /// Accepting it and ignoring it would be the quieter choice and the wrong
    /// one: a reader who wrote a name expects to be able to use it, and a read
    /// with one source answers its records under no name at all.
    fn aliased_without_a_join(&mut self) -> Error {
        self.error_here("`JOIN` — a name given with `AS` names one side of a join")
    }

    /// `FROM ( <read> )`, on its own or as the left side of a join.
    ///
    /// The inner read must state a `LIMIT`. It is materialised — there is no
    /// index to walk and no bound to push into it — so a source that could grow
    /// without limit is refused rather than cut at a number nobody wrote. A
    /// silently truncated source answers a different question from the one that
    /// was asked and looks exactly like a complete one.
    fn subquery_source(&mut self) -> Result<Source> {
        let read = self.parenthesised_read()?;
        let Some(alias) = self.alias()? else {
            if self.peek_keyword() == Some(Keyword::Join) {
                return Err(self.error_here(
                    "`AS <name>` before `JOIN` — a read has no name of its own, and a \
                     row files each side under a name",
                ));
            }
            return Ok(Source::Subquery {
                read: Box::new(read),
                condition: self.materialised_condition()?,
            });
        };
        if !self.eat_keyword(Keyword::Join) {
            return Err(self.aliased_without_a_join());
        }
        self.join(JoinSide::Read {
            read: Box::new(read),
            alias,
        })
    }

    /// `WHERE …` after a materialised source, consumed if it is there.
    ///
    /// The same clause `FROM t WHERE c` carries, in the one other position a
    /// source can stand. Its records are already in hand, so the condition
    /// narrows them rather than choosing an access path.
    fn materialised_condition(&mut self) -> Result<Option<Box<Expr>>> {
        if !self.eat_keyword(Keyword::Where) {
            return Ok(None);
        }
        Ok(Some(Box::new(self.condition()?)))
    }

    /// `( SELECT … LIMIT n )` — the read a source materialises.
    fn parenthesised_read(&mut self) -> Result<Select> {
        self.expect_punct(Punct::ParenOpen, "`(` and the read to materialise")?;
        if self.peek_keyword() != Some(Keyword::Select) {
            return Err(self.error_here("`SELECT` — a source in parentheses is a read"));
        }
        let read = self.select_statement()?;
        if read.limit.is_none() {
            return Err(self.error_here(
                "`LIMIT n` on the inner read — a materialised source states how much \
                 it may hold, so that a truncated answer is never mistaken for a whole one",
            ));
        }
        self.expect_punct(Punct::ParenClose, "`)` after the materialised read")?;
        Ok(read)
    }

    /// `SELECT <projection> FROM …`, resolving to exactly one access path.
    pub(super) fn select_statement(&mut self) -> Result<Select> {
        let start = self.span_here();
        self.advance();
        let projection = self.projection()?;
        // Beside the projection rather than among the clauses after `FROM`,
        // because it says what the star contributes and not what the read does.
        let omit = self.omit_paths(&projection)?;
        self.expect_keyword(Keyword::From, "`FROM` and what to read")?;
        // Between `FROM` and the source because that is what it qualifies: how
        // many of them there are to answer with, said before the thing itself.
        let only = self.eat_keyword(Keyword::Only).then(|| self.span_behind());

        let from = self.select_source()?;
        // Written in the order it is applied: references are followed before
        // anything groups, projects or sorts, so the clause sits before them.
        // The grammar keeps clause order and application order the same on
        // purpose — see `START` before `LIMIT` below.
        let fetch = self.fetch_paths()?;
        // After the fetch and before everything that counts records, which is
        // where it is applied: the split is what decides how many there are.
        let split = self.split_path()?;
        let group = self.group_by()?;
        let order = self.order_by()?;
        // After the order, because the order is what it resumes: the anchor is
        // the last record of the page before, and "after" is a position in the
        // sequence the clause above just named. Before `START`, which it is also
        // refused beside — both say where the page begins.
        let after = self.after_anchor()?;
        // `START` before `LIMIT`, because that is the order they are applied in
        // and a grammar that let them be written either way would suggest they
        // commute.
        let skip = self.bound("start")?;
        let limit = self.bound("limit")?;
        // Last, because it qualifies the whole read rather than any one clause,
        // and contextual like the rest: a field called `approximate` stays a
        // field.
        let approximate = self.approximation()?;
        // Beside `APPROXIMATE` because it qualifies the read the same way — both
        // say something about how the answer may be produced — and before
        // `USING`, which is an assertion about what the read then did.
        let lift_scan_guard = self.scan_guard()?;
        // After everything, because it is an assertion *about* the read rather
        // than part of it — nothing below the parser reads it to decide
        // anything. Contextual like the rest, so a field called `using` stays a
        // field.
        let using = self.using()?;
        // After `USING`, so the tail reads in the order a statement is thought
        // about: what to read, how much of it, what it should have done, and how
        // long it may take doing it.
        let timeout = self.timeout()?;
        // Last of all. It qualifies the whole read the way `USING` and `TIMEOUT`
        // do, and a reader who has taken in the question is then told which
        // state answered it.
        let version = self.version()?;
        // After `VERSION`, because it is the clause that may disagree with it:
        // a read naming one exact point in history has no room for a tolerance
        // about how old that point is.
        let staleness = self.staleness()?;
        if let (Some(_), Some(bound)) = (version.as_ref(), staleness.as_ref()) {
            return Err(Error::StalenessBesideAVersion { span: bound.span });
        }
        // Last, and deliberately not beside `STALENESS` even though the two are
        // the pair a reader will compare. They answer different questions —
        // *how old may the copy be* against *which node may answer at all* — so
        // there is no pairing rule between them to enforce here: naming both is
        // legal and the read is answered only where both hold.
        let answered_by = self.answered_by()?;
        super::shape::check_grouping(&projection, &group)?;
        super::shape::check_fold_positions(&from, &group, &order)?;
        super::shape::check_cursor(
            &from,
            after.as_deref(),
            skip,
            [
                ("GROUP BY", !group.is_empty()),
                ("FETCH", !fetch.is_empty()),
                ("SPLIT ON", split.is_some()),
            ],
        )?;
        // Where `[*]` may stand. A condition admits one on the left of a
        // comparison and a projection admits one as a whole projected value; a
        // key and an ordering do not yet, and each is refused by name rather
        // than by a stray-token message.
        if let Projection::Values { values, .. } = &projection {
            for value in values {
                super::shape::check_projected(&value.value)?;
            }
        }
        for key in &group {
            super::shape::no_several(key)?;
        }
        for ordering in &order {
            super::shape::no_several(&ordering.key)?;
        }
        for route in &fetch {
            super::shape::no_several_path(route)?;
        }
        match &from {
            Source::Where { condition, .. } => super::shape::check_several(condition)?,
            Source::Join { condition, .. } => {
                if let Some(condition) = condition {
                    super::shape::check_several(condition)?;
                }
            }
            Source::Subquery {
                condition: Some(condition),
                ..
            } => super::shape::check_several(condition)?,
            Source::Node
            | Source::Record(_)
            | Source::Table(_)
            | Source::Range { .. }
            | Source::Traverse { .. }
            | Source::Subquery { .. } => {}
        }
        Ok(Select {
            projection,
            omit,
            from,
            only,
            fetch,
            split,
            group,
            order,
            after,
            approximate,
            lift_scan_guard,
            start: skip,
            limit,
            using,
            timeout,
            version,
            staleness,
            answered_by,
            span: start.to(self.span_behind()),
        })
    }

    /// The rest of `FROM users JOIN orders ON users.id = orders.user`.
    ///
    /// # Both sides of `ON` are routes into the joined row
    ///
    /// Which is why they are written with the table in front: the row is
    /// `{ users: { … }, orders: { … } }`, so `users.id` is the path it looks
    /// like. They may be written either way round — the parser sorts out which
    /// side is which — because a reader writing the condition is thinking about
    /// the two fields and not about which table the statement named first.
    ///
    /// The root is then stripped, so what the executor holds is a route into a
    /// *record* on each side. That is what lets the right side be probed through
    /// an index, which reads records and knows nothing about a composite.
    fn join(&mut self, left: JoinSide) -> Result<Source> {
        let right = self.join_side()?;
        let on = self.span_here();
        self.expect_keyword(Keyword::On, "`ON` and the two fields to match")?;
        let first = self.field_path()?;
        self.expect_punct(Punct::Equals, "`=` between the two sides of the join")?;
        let second = self.field_path()?;

        // The two **names**, not the two tables: `users AS a JOIN users AS b` is
        // one table under two names and reads perfectly, while `users JOIN users`
        // is two names that are one and has no row a reader could address.
        if left.name() == right.name() {
            return Err(Error::OneSidedJoin {
                name: left.name().to_owned(),
                span: on.to(self.span_behind()),
            });
        }
        let sides = [&left, &right].map(|side| side.name().to_owned());
        let first_side = side_of(&first, &sides)?;
        let second_side = side_of(&second, &sides)?;
        if first_side.0 == second_side.0 {
            return Err(Error::OneSidedJoin {
                name: sides[first_side.0].clone(),
                span: on.to(self.span_behind()),
            });
        }
        let (left_key, right_key) = if first_side.0 == 0 {
            (first_side.1, second_side.1)
        } else {
            (second_side.1, first_side.1)
        };

        let condition = self
            .eat_keyword(Keyword::Where)
            .then(|| self.condition().map(Box::new))
            .transpose()?;
        Ok(Source::Join {
            left: Box::new(left),
            right: Box::new(right),
            left_key,
            right_key,
            condition,
        })
    }

    /// The side after `JOIN` — a table, or a read that must name itself.
    fn join_side(&mut self) -> Result<JoinSide> {
        if self.peek() == Some(&Token::Punct(Punct::ParenOpen)) {
            let read = self.parenthesised_read()?;
            let Some(alias) = self.alias()? else {
                return Err(self.error_here(
                    "`AS <name>` after the read — a read has no name of its own, and a \
                     row files each side under a name",
                ));
            };
            return Ok(JoinSide::Read {
                read: Box::new(read),
                alias,
            });
        }
        let table = self.table_ref()?;
        Ok(JoinSide::Table {
            table,
            alias: self.alias()?,
        })
    }

    /// The rest of `users:1->follows`, `users:1->follows->users`, or a chain of
    /// those: `users:1->follows->users->follows->users`.
    ///
    /// **Every arrow points the same way.** Within a step a mixed pair would read
    /// as "the edges out of `a`, then whichever record their `out` names" — which
    /// is `a` again, for every edge, and is a query nobody means to write. Across
    /// steps a mixed pair asks a real question, and it is a design rather than a
    /// loosened rule; `docs/tessariql.md` §8 holds it as its own row.
    ///
    /// The loop is what keeps `a->e1->e2` unambiguous: a table read after an
    /// arrow is this step's **node**, and the walk continues only if another
    /// arrow follows it. So there is never a step with a gap where its node
    /// should be.
    fn traversal(&mut self, from: RecordTarget, direction: Direction) -> Result<Source> {
        let mut hops = Vec::new();
        loop {
            let edges = self.table_ref()?;
            let Some(second) = self.arrow() else {
                hops.push(Hop {
                    edges,
                    target: None,
                });
                break;
            };
            if second != direction {
                return Err(self.error_here("an arrow pointing the same way as the first"));
            }
            let target = self.table_ref()?;
            hops.push(Hop {
                edges,
                target: Some(target),
            });
            match self.arrow() {
                Some(next) if next == direction => {}
                Some(_) => {
                    return Err(self.error_here("an arrow pointing the same way as the first"));
                }
                None => break,
            }
        }
        let depth = self.depth_bound(&hops)?;
        Ok(Source::Traverse {
            from,
            direction,
            hops,
            depth,
        })
    }

    /// One traversal arrow, consumed if it is there.
    fn arrow(&mut self) -> Option<Direction> {
        if self.eat_punct(Punct::ArrowRight) {
            return Some(Direction::Outgoing);
        }
        if self.eat_punct(Punct::ArrowLeft) {
            return Some(Direction::Incoming);
        }
        None
    }

    /// `RELATE users:1->follows->users:2 = { since: … }`
    ///
    /// The `= { … }` is optional, because most edges carry nothing but their two
    /// endpoints, and a required empty object would be noise on every line.
    fn relate_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        let from = self.record_target()?;
        self.expect_punct(Punct::ArrowRight, "`->` and the edge table")?;
        let edges = self.table_ref()?;
        self.expect_punct(Punct::ArrowRight, "`->` and the record to relate to")?;
        let to = self.record_target()?;
        let value = if self.eat_punct(Punct::Equals) {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        Ok(StatementKind::Relate {
            from,
            edges,
            to,
            value,
        })
    }

    /// `KEYS FROM sessions RANGE 'a'..'m'`
    fn keys_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        self.expect_keyword(Keyword::From, "`FROM` and the space to list")?;
        let space = self.table_ref()?;
        let range = if self.eat_keyword(Keyword::Range) {
            Some(self.range()?)
        } else {
            None
        };
        Ok(StatementKind::Keys { space, range })
    }

    /// The range after `RANGE`, which must be one.
    fn range(&mut self) -> Result<RangeExpr> {
        let start = self.span_here();
        let expression = self.expression()?;
        match expression.kind {
            ExprKind::Range(range) => Ok(range),
            _ => Err(Error::NotARange {
                span: start.to(self.span_behind()),
            }),
        }
    }
}
