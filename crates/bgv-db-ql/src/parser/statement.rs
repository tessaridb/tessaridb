//! One statement at a time.

use super::Parser;
use bgv_db_types::{FieldKind, Filter, Path, Step};

use crate::ast::{
    Assignment, Direction, Edit, ExprKind, FieldPath, Hop, InfoSubject, Projection, RangeExpr,
    RecordTarget, Select, Source, Statement, StatementKind, TableRef,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

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
            Some(Keyword::Rebuild) => self.rebuild_statement()?,
            Some(Keyword::Grant) => self.grant_statement(true)?,
            Some(Keyword::Revoke) => self.grant_statement(false)?,
            Some(Keyword::Create) => self.write_statement(Keyword::Create)?,
            Some(Keyword::Update) => self.write_statement(Keyword::Update)?,
            Some(Keyword::Set) => self.write_statement(Keyword::Set)?,
            Some(Keyword::Select) => StatementKind::Select(self.select_statement()?),
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
                    self.expect_keyword(Keyword::Where, "`WHERE` and what to remove")?;
                    let condition = self.condition()?;
                    super::shape::no_fold(&condition)?;
                    super::shape::check_several(&condition)?;
                    StatementKind::DeleteWhere {
                        table,
                        condition: Box::new(condition),
                    }
                } else {
                    StatementKind::Delete {
                        target: self.record_target()?,
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
                    let Some(Token::Number(bgv_db_types::Number::Integer(held))) = self.peek()
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
            _ => return Err(self.error_here("a statement")),
        };
        Ok(Statement {
            kind,
            span: start.to(self.span_behind()),
        })
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
            Some(Keyword::Database) => {
                self.advance();
                InfoSubject::Database
            }
            Some(Keyword::Table) => {
                self.advance();
                InfoSubject::Table(self.table_ref()?)
            }
            Some(Keyword::User) => {
                self.advance();
                InfoSubject::User(self.name()?)
            }
            _ if self.eat_word("store") => InfoSubject::Store,
            _ if self.eat_word("node") => InfoSubject::Node,
            _ => {
                return Err(
                    self.error_here("`STORE`, `NAMESPACE`, `DATABASE`, `TABLE`, `USER` or `NODE`")
                );
            }
        };
        Ok(StatementKind::Info { subject })
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
        if namespace.is_none() && database.is_none() {
            return Err(self.error_here("`NAMESPACE` or `DATABASE` after `USE`"));
        }
        Ok(StatementKind::Use {
            namespace,
            database,
        })
    }

    fn define_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        match self.peek_keyword() {
            Some(Keyword::Namespace) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineNamespace {
                    name: self.name()?,
                    if_not_exists,
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
                // Either marker, in either order, and neither twice. Order-free
                // because there is no reading under which one has to precede the
                // other, and a grammar that insisted would only be remembered
                // wrong.
                let mut schemafull = false;
                let mut edge = false;
                loop {
                    if !schemafull && self.eat_keyword(Keyword::Schemafull) {
                        schemafull = true;
                    } else if !edge && self.eat_keyword(Keyword::Edge) {
                        edge = true;
                    } else {
                        break;
                    }
                }
                Ok(StatementKind::DefineTable {
                    name,
                    schemafull,
                    edge,
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
            Some(Keyword::Bucket) => {
                self.advance();
                let if_not_exists = self.eat_if_not_exists()?;
                Ok(StatementKind::DefineBucket {
                    name: self.name()?,
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
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `BUCKET`, `INDEX`, `FIELD`, `ANALYZER`, `USER`, `NODE` or `REPLICA`",
            )),
        }
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
    /// to drop the roles, and there is no spelling for removing one role —
    /// which would need a spelling for removing the last one, a question worth
    /// answering when there is a second node to answer it against.
    fn define_node(&mut self) -> Result<StatementKind> {
        let roles = if self.eat_word("roles") {
            let mut named = vec![self.name()?];
            while self.eat_punct(Punct::Comma) {
                named.push(self.name()?);
            }
            Some(named)
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
        if roles.is_none() && endpoints.is_none() {
            return Err(self.error_here("`ROLES` or `ENDPOINTS` and what to set"));
        }
        Ok(StatementKind::DefineNode { roles, endpoints })
    }

    /// `DEFINE REPLICA second AT 'host:9001' ROLES serving, writable`
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
    fn define_replica(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("at") {
            return Err(self.error_here("`AT` and where the peer is reached"));
        }
        let (endpoint, _) = self.text("the endpoint, as text")?;
        let roles = if self.eat_word("roles") {
            let mut named = vec![self.name()?];
            while self.eat_punct(Punct::Comma) {
                named.push(self.name()?);
            }
            Some(named)
        } else {
            None
        };
        Ok(StatementKind::DefineReplica {
            name,
            endpoint,
            roles,
            if_not_exists,
        })
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
        // **One** marker, and the third is why this changed. `UNIQUE` says how
        // entries collide, `SEARCH` says the entries are terms, `VECTOR` says
        // they are a graph — three different index kinds wearing three flags, of
        // which at most one can be true. Accepting two used to be possible and
        // the first one checked simply won, so `UNIQUE SEARCH` was an index
        // whose uniqueness was silently ignored. Adding a third made that
        // inconsistency a thing to answer rather than inherit.
        /// Which of the three an index is.
        enum Marker {
            Unique,
            Search,
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
                Some(Marker::Search | Marker::Vector(_)) => {
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
            Some(self.table_ref()?)
        } else {
            None
        };
        self.expect_keyword(Keyword::Role, "`ROLE` and what the user may do")?;
        let role = self.name()?;
        self.expect_keyword(Keyword::Password, "`PASSWORD` and the credential")?;
        let (password, _) = self.text("the password, as text")?;
        Ok(StatementKind::DefineUser {
            name,
            scope,
            role,
            password,
            if_not_exists,
        })
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

    /// `DEFINE FIELD email ON users TYPE string`
    fn define_field(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the field is on")?;
        let table = self.table_ref()?;
        self.expect_keyword(Keyword::Type, "`TYPE` and what the field may hold")?;
        let kind = self.field_kind()?;
        // Either marker, in either order, and neither twice — the rule
        // `DEFINE TABLE`'s two flags already follow, for the same reason: there
        // is no reading under which one has to precede the other, and a grammar
        // that insisted would only be remembered wrong.
        let mut required = false;
        let mut default = None;
        let mut analyzer = None;
        let mut assert = None;
        loop {
            if !required && self.eat_keyword(Keyword::Required) {
                required = true;
            } else if default.is_none() && self.eat_keyword(Keyword::Default) {
                default = Some(self.written_expression()?);
            } else if analyzer.is_none() && self.eat_keyword(Keyword::Analyzer) {
                analyzer = Some(self.name()?);
            } else if assert.is_none() && self.eat_word("assert") {
                // Contextual, like `vector` and `fetch`: nothing but this marker
                // can stand here, and a field called `assert` is not a name to
                // take away from a table that has one.
                assert = Some(super::assertion::lower(&self.condition()?)?);
            } else {
                break;
            }
        }
        Ok(StatementKind::DefineField {
            name,
            table,
            kind,
            required,
            default,
            analyzer,
            assert,
            if_not_exists,
        })
    }

    /// A type name, which may be spelled with a reserved word.
    ///
    /// `table`, `set`, `range`, `datetime` and `uuid` are all reserved
    /// elsewhere, and after `TYPE` nothing but a type name can appear — so the
    /// word is read as text here, the way a field name inside an object literal
    /// already is. The alternative is a language where five of the seventeen
    /// types cannot be written down.
    fn field_kind(&mut self) -> Result<FieldKind> {
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

    fn drop_statement(&mut self) -> Result<StatementKind> {
        self.advance();
        match self.peek_keyword() {
            Some(Keyword::Table | Keyword::Space) => {
                self.advance();
                Ok(StatementKind::DropTable {
                    table: self.table_ref()?,
                })
            }
            Some(Keyword::User) => {
                self.advance();
                Ok(StatementKind::DropUser { name: self.name()? })
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
            _ => Err(self.error_here("`TABLE`, `SPACE`, `INDEX` or `FIELD`")),
        }
    }

    /// The three statements shaped `<verb> <target> = <value>`.
    fn write_statement(&mut self, verb: Keyword) -> Result<StatementKind> {
        self.advance();
        let target = self.record_target()?;
        // `SET` is a key-value verb elsewhere and a clause here, which is the
        // trick this grammar already plays with `ORDER`, `FETCH` and `VECTOR`:
        // nothing but a clause can stand in this position, so nothing is
        // ambiguous, and a field called `set` keeps working.
        if verb == Keyword::Update && self.eat_keyword(Keyword::Set) {
            let mut assignments = vec![self.assignment()?];
            while self.eat_punct(Punct::Comma) {
                assignments.push(self.assignment()?);
            }
            return Ok(StatementKind::Update {
                target,
                edit: Edit::Fields(assignments),
            });
        }
        self.expect_punct(Punct::Equals, "`=` and the value to write")?;
        let value = self.expression()?;
        Ok(match verb {
            Keyword::Update => StatementKind::Update {
                target,
                edit: Edit::Whole(value),
            },
            Keyword::Set => StatementKind::Set { target, value },
            _ => StatementKind::Create { target, value },
        })
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
        self.expect_keyword(Keyword::On, "`ON` and the table")?;
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
        let table = self.table_ref()?;
        if self.peek() == Some(&Token::Punct(Punct::Colon)) {
            let record = self.record_target_after(table)?;
            return Ok(match self.arrow() {
                Some(direction) => self.traversal(record, direction)?,
                None => Source::Record(record),
            });
        }
        if self.eat_keyword(Keyword::Join) {
            return self.join(table);
        }
        if self.eat_keyword(Keyword::Where) {
            return Ok(Source::Where {
                table,
                condition: Box::new(self.condition()?),
            });
        }
        Ok(Source::Table(table))
    }

    /// `SELECT <projection> FROM …`, resolving to exactly one access path.
    pub(super) fn select_statement(&mut self) -> Result<Select> {
        let start = self.span_here();
        self.advance();
        let projection = self.projection()?;
        self.expect_keyword(Keyword::From, "`FROM` and what to read")?;

        let from = self.select_source()?;
        // Written in the order it is applied: references are followed before
        // anything groups, projects or sorts, so the clause sits before them.
        // The grammar keeps clause order and application order the same on
        // purpose — see `START` before `LIMIT` below.
        let fetch = self.fetch_paths()?;
        let group = self.group_by()?;
        let order = self.order_by()?;
        // `START` before `LIMIT`, because that is the order they are applied in
        // and a grammar that let them be written either way would suggest they
        // commute.
        let skip = self.bound("start")?;
        let limit = self.bound("limit")?;
        // Last, because it qualifies the whole read rather than any one clause,
        // and contextual like the rest: a field called `approximate` stays a
        // field.
        let approximate = self.eat_word("approximate");
        super::shape::check_grouping(&projection, &group)?;
        super::shape::check_fold_positions(&from, &group, &order)?;
        // Where `[*]` may stand. A condition admits one on the left of a
        // comparison and a projection admits one as a whole projected value; a
        // key and an ordering do not yet, and each is refused by name rather
        // than by a stray-token message.
        if let Projection::Values(values) = &projection {
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
            Source::Node | Source::Record(_) | Source::Table(_) | Source::Traverse { .. } => {}
        }
        Ok(Select {
            projection,
            from,
            fetch,
            group,
            order,
            approximate,
            start: skip,
            limit,
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
    fn join(&mut self, left: TableRef) -> Result<Source> {
        let right = self.table_ref()?;
        let on = self.span_here();
        self.expect_keyword(Keyword::On, "`ON` and the two fields to match")?;
        let first = self.field_path()?;
        self.expect_punct(Punct::Equals, "`=` between the two sides of the join")?;
        let second = self.field_path()?;

        if left.name.text == right.name.text {
            return Err(Error::OneSidedJoin {
                name: left.name.text.clone(),
                span: on.to(self.span_behind()),
            });
        }
        let sides = [&left, &right].map(|table| table.name.text.clone());
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
            left,
            right,
            left_key,
            right_key,
            condition,
        })
    }

    /// The rest of `users:1->follows`, `users:1->follows->users`, or a chain of
    /// those: `users:1->follows->users->follows->users`.
    ///
    /// **Every arrow points the same way.** Within a step a mixed pair would read
    /// as "the edges out of `a`, then whichever record their `out` names" — which
    /// is `a` again, for every edge, and is a query nobody means to write. Across
    /// steps a mixed pair asks a real question, and it is a design rather than a
    /// loosened rule; `docs/bgvql.md` §8 holds it as its own row.
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
        Ok(Source::Traverse {
            from,
            direction,
            hops,
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
            Some(self.expression()?)
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
