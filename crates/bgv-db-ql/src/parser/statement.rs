//! One statement at a time.

use super::Parser;
use bgv_db_types::FieldKind;

use crate::ast::{ExprKind, Name, RangeExpr, Select, Source, Statement, StatementKind, Test};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Spanned, Token};

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
                Ok(StatementKind::DefineTable {
                    name,
                    schemafull: self.eat_keyword(Keyword::Schemafull),
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
            _ => {
                Err(self
                    .error_here("`NAMESPACE`, `DATABASE`, `TABLE`, `SPACE`, `INDEX` or `FIELD`"))
            }
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

        let mut fields = vec![self.name()?];
        while self.eat_punct(Punct::Comma) {
            fields.push(self.name()?);
        }
        let unique = self.eat_keyword(Keyword::Unique);
        Ok(StatementKind::DefineIndex {
            name,
            table,
            fields,
            unique,
            if_not_exists,
        })
    }

    /// `DEFINE FIELD email ON users TYPE string`
    fn define_field(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the field is on")?;
        let table = self.table_ref()?;
        self.expect_keyword(Keyword::Type, "`TYPE` and what the field may hold")?;
        Ok(StatementKind::DefineField {
            name,
            table,
            kind: self.field_kind()?,
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

    /// `SELECT * FROM …`, resolving to exactly one access path.
    pub(super) fn select_statement(&mut self) -> Result<Select> {
        let start = self.span_here();
        self.advance();
        if !self.eat_punct(Punct::Star) {
            // A named projection is refused rather than accepted and ignored,
            // which would return every field to a caller that asked for one.
            return Err(Error::Unsupported {
                feature: "projecting named fields",
                span: self.span_here(),
            });
        }
        self.expect_keyword(Keyword::From, "`FROM` and what to read")?;

        let table = self.table_ref()?;
        let from = if self.peek() == Some(&Token::Punct(Punct::Colon)) {
            Source::Record(self.record_target_after(table)?)
        } else if self.eat_keyword(Keyword::Where) {
            let field = self.name()?;
            let test = if self.eat_keyword(Keyword::Like) {
                Test::Like
            } else if self.eat_keyword(Keyword::Ilike) {
                Test::Ilike
            } else if self.eat_keyword(Keyword::Contains) {
                Test::Contains
            } else {
                self.expect_punct(Punct::Equals, "`=`, `LIKE`, `ILIKE` or `CONTAINS`")?;
                Test::Equals
            };
            Source::Filter {
                table,
                field,
                test,
                value: Box::new(self.expression()?),
            }
        } else {
            Source::Table(table)
        };
        Ok(Select {
            from,
            span: start.to(self.span_behind()),
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

    /// A bare name, which is never a keyword.
    pub(super) fn name(&mut self) -> Result<Name> {
        if !matches!(self.peek(), Some(Token::Ident(_))) {
            return Err(self.error_here("a name"));
        }
        let Some(Spanned {
            token: Token::Ident(text),
            span,
        }) = self.advance()
        else {
            return Err(self.error_here("a name"));
        };
        Ok(Name { text, span })
    }
}
