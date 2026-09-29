//! Tables, records, fields, objects and embedded reads named inside an expression.

use super::super::Parser;
use crate::ast::{Expr, ExprKind, Field, Identity, Name, RecordTarget, TableRef};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Span, Spanned, Token};
use tessari_types::{Number, RecordId, parse_uuid};

impl Parser<'_> {
    /// `users` names a table; `users:1` names a record.
    pub(crate) fn table_or_record(&mut self) -> Result<Expr> {
        let table = self.table_ref()?;
        if self.peek() != Some(&Token::Punct(Punct::Colon)) {
            let span = table.span;
            return Ok(Expr {
                kind: ExprKind::Table(table),
                span,
            });
        }
        let target = self.record_target_after(table)?;
        let span = target.span;
        Ok(Expr {
            kind: ExprKind::Record(target),
            span,
        })
    }

    /// A comma-separated list, with a trailing comma allowed.
    pub(crate) fn items(&mut self, close: Punct, open: Span) -> Result<(Vec<Expr>, Span)> {
        self.advance();
        let mut items = Vec::new();
        loop {
            if self.eat_punct(close) {
                return Ok((items, open.to(self.span_behind())));
            }
            items.push(self.expression()?);
            if !self.eat_punct(Punct::Comma) {
                let end = self.expect_punct(close, "`,` or the end of the list")?;
                return Ok((items, open.to(end)));
            }
        }
    }

    pub(crate) fn object(&mut self, open: Span) -> Result<Expr> {
        self.advance();
        let mut fields: Vec<Field> = Vec::new();
        loop {
            if self.eat_punct(Punct::BraceClose) {
                break;
            }
            let name = self.field_name()?;
            if fields.iter().any(|field| field.name.text == name.text) {
                return Err(Error::DuplicateField {
                    name: name.text,
                    span: name.span,
                });
            }
            self.expect_punct(Punct::Colon, "`:` and the field's value")?;
            let value = self.expression()?;
            fields.push(Field { name, value });
            if !self.eat_punct(Punct::Comma) {
                self.expect_punct(Punct::BraceClose, "`,` or `}`")?;
                break;
            }
        }
        Ok(Expr {
            kind: ExprKind::Object(fields),
            span: open.to(self.span_behind()),
        })
    }

    /// A field name.
    ///
    /// A reserved word is accepted here, and that is not the contextual-keyword
    /// trap it looks like: a field name is always followed by `:` and can never
    /// be a verb in this position, so nothing about the grammar depends on where
    /// the reader is standing. What it buys is that `unique`, `where`, `range`,
    /// `index` and `table` stay usable as what they usually are — ordinary words
    /// in someone's data. The name is taken from the **source text** rather than
    /// the keyword's spelling, because a field name is case-sensitive and the
    /// keyword is not.
    ///
    /// Text is also accepted, for a name that is not a word at all.
    pub(crate) fn field_name(&mut self) -> Result<Name> {
        if matches!(self.peek(), Some(Token::Str(_))) {
            let (text, span) = self.quoted_field_name()?;
            return Ok(Name { text, span });
        }
        if matches!(self.peek(), Some(Token::Keyword(_))) {
            let span = self.span_here();
            self.advance();
            let text = self.source.get(span.start..span.end).unwrap_or_default();
            return Ok(Name {
                text: text.to_owned(),
                span,
            });
        }
        self.name()
    }

    pub(crate) fn quoted_field_name(&mut self) -> Result<(String, Span)> {
        let Some(Spanned {
            token: Token::Str(text),
            span,
        }) = self.advance()
        else {
            return Err(self.error_here("a field name"));
        };
        Ok((text, span))
    }

    /// `(SELECT * FROM users:1)` — the only thing parentheses hold, because
    /// there are no operators to group.
    /// What stands between parentheses: an embedded read, or a grouping.
    ///
    /// Grouping exists because precedence exists. `a AND (b OR c)` has to be
    /// writable the moment `AND` binds tighter than `OR`, and a language whose
    /// precedence cannot be overridden makes the author restructure the query
    /// instead of saying what they mean.
    pub(crate) fn embedded_select(&mut self, open: Span) -> Result<Expr> {
        self.advance();
        if self.peek_keyword() != Some(Keyword::Select) {
            let inner = self.expression()?;
            let end = self.expect_punct(Punct::ParenClose, "`)` after the expression")?;
            // The span covers the parentheses, so a failure inside a group
            // points at the group rather than at one token of it.
            return Ok(Expr {
                kind: inner.kind,
                span: open.to(end),
            });
        }
        let select = self.select_statement()?;
        let end = self.expect_punct(Punct::ParenClose, "`)` after the embedded read")?;
        Ok(Expr {
            kind: ExprKind::Select(Box::new(select)),
            span: open.to(end),
        })
    }

    /// A table, optionally qualified by its database: `orders.users`.
    pub(crate) fn table_ref(&mut self) -> Result<TableRef> {
        let first = self.name()?;
        if !self.eat_punct(Punct::Dot) {
            let span = first.span;
            return Ok(TableRef {
                database: None,
                name: first,
                span,
            });
        }
        let second = self.name()?;
        let span = first.span.to(second.span);
        Ok(TableRef {
            database: Some(first),
            name: second,
            span,
        })
    }

    /// The `:id` half, once the table is already read.
    pub(crate) fn record_target_after(&mut self, table: TableRef) -> Result<RecordTarget> {
        self.expect_punct(Punct::Colon, "`:` and the record's identity")?;
        let at = self.span_here();
        let id = self.record_id(at)?;
        Ok(RecordTarget {
            span: table.span.to(self.span_behind()),
            table,
            id,
        })
    }

    /// The four kinds a record id has, and nothing else.
    ///
    /// A float is refused rather than converted: `users:1.0` and `users:1` would
    /// otherwise be one record or two depending on how the text was written.
    pub(crate) fn record_id(&mut self, at: Span) -> Result<Identity> {
        if self.peek_keyword() == Some(Keyword::Uuid) {
            let (text, span) = self.marked_string("text after `uuid`")?;
            let bytes = parse_uuid(&text).ok_or(Error::InvalidUuid { text, span })?;
            return Ok(Identity::Fixed(RecordId::Uuid(bytes)));
        }
        let Some(spanned) = self.advance() else {
            return Err(Error::InvalidRecordId { span: at });
        };
        match spanned.token {
            Token::Number(Number::Integer(value)) => Ok(Identity::Fixed(RecordId::Int(value))),
            Token::Str(text) => Ok(Identity::Fixed(RecordId::Text(text))),
            Token::Bytes(bytes) => Ok(Identity::Fixed(RecordId::Bytes(bytes))),
            // The table half of `table:id` is a name and the id half is a value,
            // which is why a parameter stands here and never one step left.
            Token::Parameter(name) => Ok(Identity::Parameter(name)),
            _ => Err(Error::InvalidRecordId { span: spanned.span }),
        }
    }
}
