//! One statement at a time.

use super::Parser;
use bgv_db_types::{FieldKind, Filter, Path, Step};

use crate::ast::{
    Direction, ExprKind, FieldPath, RangeExpr, RecordTarget, Select, Source, Statement,
    StatementKind, TableRef,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

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
            Some(Keyword::Create) => self.write_statement(Keyword::Create)?,
            Some(Keyword::Update) => self.write_statement(Keyword::Update)?,
            Some(Keyword::Set) => self.write_statement(Keyword::Set)?,
            Some(Keyword::Select) => StatementKind::Select(self.select_statement()?),
            Some(Keyword::Delete) => {
                self.advance();
                // `FROM` is what tells the two forms apart, and it is required
                // for the conditional one: `DELETE readings WHERE …` would read
                // as a table name where an identity belongs, and a statement
                // that removes rows should not be one word away from a typo.
                if self.eat_keyword(Keyword::From) {
                    let table = self.table_ref()?;
                    self.expect_keyword(Keyword::Where, "`WHERE` and what to remove")?;
                    StatementKind::DeleteWhere {
                        table,
                        condition: Box::new(self.condition()?),
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
            Some(Keyword::Index) => self.define_index(),
            Some(Keyword::Field) => self.define_field(),
            Some(Keyword::Analyzer) => self.define_analyzer(),
            Some(Keyword::User) => self.define_user(),
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `INDEX`, `FIELD`, `ANALYZER` or `USER`",
            )),
        }
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
        loop {
            if !required && self.eat_keyword(Keyword::Required) {
                required = true;
            } else if default.is_none() && self.eat_keyword(Keyword::Default) {
                default = Some(self.written_expression()?);
            } else if analyzer.is_none() && self.eat_keyword(Keyword::Analyzer) {
                analyzer = Some(self.name()?);
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
        self.expect_punct(Punct::Equals, "`=` and the value to write")?;
        let value = self.expression()?;
        Ok(match verb {
            Keyword::Update => StatementKind::Update { target, value },
            Keyword::Set => StatementKind::Set { target, value },
            _ => StatementKind::Create { target, value },
        })
    }

    /// `SELECT <projection> FROM …`, resolving to exactly one access path.
    pub(super) fn select_statement(&mut self) -> Result<Select> {
        let start = self.span_here();
        self.advance();
        let projection = self.projection()?;
        self.expect_keyword(Keyword::From, "`FROM` and what to read")?;

        let table = self.table_ref()?;
        let from = if self.peek() == Some(&Token::Punct(Punct::Colon)) {
            let record = self.record_target_after(table)?;
            match self.arrow() {
                Some(direction) => self.traversal(record, direction)?,
                None => Source::Record(record),
            }
        } else if self.eat_keyword(Keyword::Join) {
            self.join(table)?
        } else if self.eat_keyword(Keyword::Where) {
            Source::Where {
                table,
                condition: Box::new(self.condition()?),
            }
        } else {
            Source::Table(table)
        };
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

    /// The rest of `users:1->follows` or `users:1->follows->users`.
    ///
    /// The second arrow must point the same way as the first. A mixed pair would
    /// read as "the edges out of `a`, then whichever record their `out` names" —
    /// which is `a` again, for every edge, and is a query nobody means to write.
    fn traversal(&mut self, from: RecordTarget, direction: Direction) -> Result<Source> {
        let edges = self.table_ref()?;
        let target = match self.arrow() {
            Some(second) if second == direction => Some(self.table_ref()?),
            Some(_) => {
                return Err(self.error_here("a second arrow pointing the same way as the first"));
            }
            None => None,
        };
        Ok(Source::Traverse {
            from,
            direction,
            edges,
            target,
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
