//! One statement at a time.

use super::Parser;
use bgv_db_types::{FieldKind, Filter};

use crate::ast::{
    Direction, ExprKind, RangeExpr, RecordTarget, Select, Source, Statement, StatementKind,
};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

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
                StatementKind::Delete {
                    target: self.record_target()?,
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
            _ => Err(self.error_here(
                "`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `INDEX`, `FIELD` or `ANALYZER`",
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
        // Either marker, in either order, and neither twice — the rule the
        // table and field declarations already follow.
        let mut unique = false;
        let mut search = false;
        loop {
            if !unique && self.eat_keyword(Keyword::Unique) {
                unique = true;
            } else if !search && self.eat_keyword(Keyword::Search) {
                search = true;
            } else {
                break;
            }
        }
        Ok(StatementKind::DefineIndex {
            name,
            table,
            fields,
            unique,
            search,
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
        } else if self.eat_keyword(Keyword::Where) {
            Source::Where {
                table,
                condition: Box::new(self.condition()?),
            }
        } else {
            Source::Table(table)
        };
        let group = self.group_by()?;
        let order = self.order_by()?;
        // `START` before `LIMIT`, because that is the order they are applied in
        // and a grammar that let them be written either way would suggest they
        // commute.
        let skip = self.bound("start")?;
        let limit = self.bound("limit")?;
        super::shape::check_grouping(&projection, &group)?;
        Ok(Select {
            projection,
            from,
            group,
            order,
            start: skip,
            limit,
            span: start.to(self.span_behind()),
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
