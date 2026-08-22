//! Values, containers, and the two reads that may stand where a value stands.

use bgv_db_types::{Datetime, Number, RecordId, Value, parse_uuid};
use rust_decimal::Decimal;

use super::Parser;
use crate::ast::{Expr, ExprKind, Field, Identity, Name, RangeExpr, RecordTarget, TableRef};
use crate::error::{Error, Result};
use crate::function::Function;
use crate::token::{Keyword, Punct, Span, Spanned, Token};

impl Parser<'_> {
    /// A value or a test, at the loosest binding.
    ///
    /// Precedence, loosest first: `OR`, `AND`, `NOT`, then the comparisons —
    /// which are **non-associative**, so `a < b < c` is refused rather than read
    /// as one of the two things it might mean — then a range, then a primary.
    /// Parentheses override, as everywhere.
    pub(super) fn expression(&mut self) -> Result<Expr> {
        self.disjunction()
    }

    /// A value, possibly a range of two.
    pub(super) fn spanned_range(&mut self) -> Result<Expr> {
        let start = self.primary()?;
        let inclusive = if self.eat_punct(Punct::DotDot) {
            false
        } else if self.eat_punct(Punct::DotDotEquals) {
            true
        } else {
            return Ok(start);
        };
        let end = self.primary()?;
        let span = start.span.to(end.span);
        Ok(Expr {
            kind: ExprKind::Range(RangeExpr {
                start: Box::new(start),
                end: Box::new(end),
                inclusive,
            }),
            span,
        })
    }

    fn primary(&mut self) -> Result<Expr> {
        let span = self.span_here();
        // A call is recognised before a keyword is, so `type::of(x)` reads as
        // the function it obviously is. A reserved word before `::` cannot be
        // anything else, which is the same reasoning that lets one stand after
        // `TYPE` and inside an object literal.
        if self.call_follows() {
            return self.call(span);
        }
        if let Some(keyword) = self.peek_keyword() {
            return self.keyword_value(keyword, span);
        }
        match self.peek() {
            // The one token whose meaning depends on where it stands: a route
            // into the record in a condition, a table in a value position.
            Some(Token::Ident(_)) if self.reading_paths && !self.record_follows() => {
                let path = self.field_path()?;
                let span = path.span;
                Ok(Expr {
                    kind: ExprKind::Path(path),
                    span,
                })
            }
            // A parameter reads the same in both positions, which is the point:
            // it is a value, so a caller who can supply one cannot thereby name
            // a field, a table or a route into a record.
            Some(Token::Parameter(name)) => {
                let kind = ExprKind::Parameter(name.clone());
                self.advance();
                Ok(Expr { kind, span })
            }
            Some(Token::Ident(_)) => self.table_or_record(),
            Some(Token::Punct(Punct::BracketOpen)) => {
                let (items, span) = self.items(Punct::BracketClose, span)?;
                Ok(Expr {
                    kind: ExprKind::Array(items),
                    span,
                })
            }
            Some(Token::Punct(Punct::BraceOpen)) => self.object(span),
            Some(Token::Punct(Punct::ParenOpen)) => self.embedded_select(span),
            Some(_) => self.literal_token(span),
            None => Err(self.error_here("a value")),
        }
    }

    /// The values a keyword introduces, and the reads that are also values.
    fn keyword_value(&mut self, keyword: Keyword, span: Span) -> Result<Expr> {
        let literal = match keyword {
            Keyword::None => Value::None,
            Keyword::Null => Value::Null,
            Keyword::True => Value::Bool(true),
            Keyword::False => Value::Bool(false),
            Keyword::Dec => return self.decimal(span),
            Keyword::Datetime => return self.datetime(span),
            Keyword::Uuid => return self.uuid(span),
            Keyword::Set => {
                self.advance();
                let open = self.span_here();
                if self.peek() != Some(&Token::Punct(Punct::BracketOpen)) {
                    return Err(self.error_here("`[` — a set is written `set [a, b]`"));
                }
                let (items, closed) = self.items(Punct::BracketClose, open)?;
                return Ok(Expr {
                    kind: ExprKind::Set(items),
                    span: span.to(closed),
                });
            }
            Keyword::Get => {
                self.advance();
                let target = self.record_target()?;
                let end = target.span;
                return Ok(Expr {
                    kind: ExprKind::Get(target),
                    span: span.to(end),
                });
            }
            _ => return Err(self.error_here("a value")),
        };
        self.advance();
        Ok(Expr {
            kind: ExprKind::Literal(literal),
            span,
        })
    }

    /// `dec 12.34` — read from the characters written, not from the float the
    /// lexer produced, because converting that float back is exactly the
    /// rounding the marker exists to prevent.
    fn decimal(&mut self, span: Span) -> Result<Expr> {
        self.advance();
        let number = self.span_here();
        if !matches!(self.peek(), Some(Token::Number(_))) {
            return Err(self.error_here("a number after `dec`"));
        }
        self.advance();
        let text = self
            .source
            .get(number.start..number.end)
            .unwrap_or_default();
        let value = Decimal::from_str_exact(text).map_err(|_| Error::InvalidDecimal {
            text: text.to_owned(),
            span: number,
        })?;
        Ok(Expr {
            kind: ExprKind::Literal(Value::Number(Number::Decimal(value))),
            span: span.to(number),
        })
    }

    fn datetime(&mut self, span: Span) -> Result<Expr> {
        let (text, at) = self.marked_string("text after `datetime`")?;
        let value = Datetime::parse_rfc3339(&text).ok_or(Error::InvalidDatetime {
            text: text.clone(),
            span: at,
        })?;
        Ok(Expr {
            kind: ExprKind::Literal(Value::Datetime(value)),
            span: span.to(at),
        })
    }

    fn uuid(&mut self, span: Span) -> Result<Expr> {
        let (text, at) = self.marked_string("text after `uuid`")?;
        let value = parse_uuid(&text).ok_or(Error::InvalidUuid {
            text: text.clone(),
            span: at,
        })?;
        Ok(Expr {
            kind: ExprKind::Literal(Value::Uuid(value)),
            span: span.to(at),
        })
    }

    /// The string a marker keyword applies to.
    fn marked_string(&mut self, expected: &'static str) -> Result<(String, Span)> {
        // Past the marker — `datetime`, `uuid` — which the caller has peeked at
        // and not consumed.
        self.advance();
        self.text(expected)
    }

    /// A string literal standing where one is required.
    pub(super) fn text(&mut self, expected: &'static str) -> Result<(String, Span)> {
        if !matches!(self.peek(), Some(Token::Str(_))) {
            return Err(self.error_here(expected));
        }
        let Some(Spanned {
            token: Token::Str(text),
            span,
        }) = self.advance()
        else {
            return Err(self.error_here(expected));
        };
        Ok((text, span))
    }

    /// A literal the lexer already resolved to a whole value.
    fn literal_token(&mut self, span: Span) -> Result<Expr> {
        let Some(spanned) = self.advance() else {
            return Err(self.error_here("a value"));
        };
        let literal = match spanned.token {
            Token::Number(number) => Value::Number(number),
            Token::Str(text) => Value::String(text),
            Token::Bytes(bytes) => Value::Bytes(bytes),
            Token::Duration(duration) => Value::Duration(duration),
            _ => {
                return Err(Error::UnexpectedToken {
                    found: super::describe(&spanned.token),
                    expected: "a value",
                    span: spanned.span,
                });
            }
        };
        Ok(Expr {
            kind: ExprKind::Literal(literal),
            span,
        })
    }

    /// `users` names a table; `users:1` names a record.
    fn table_or_record(&mut self) -> Result<Expr> {
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
    fn items(&mut self, close: Punct, open: Span) -> Result<(Vec<Expr>, Span)> {
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

    fn object(&mut self, open: Span) -> Result<Expr> {
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
    fn field_name(&mut self) -> Result<Name> {
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

    fn quoted_field_name(&mut self) -> Result<(String, Span)> {
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
    fn embedded_select(&mut self, open: Span) -> Result<Expr> {
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
    pub(super) fn table_ref(&mut self) -> Result<TableRef> {
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

    pub(super) fn record_target(&mut self) -> Result<RecordTarget> {
        let table = self.table_ref()?;
        self.record_target_after(table)
    }

    /// The `:id` half, once the table is already read.
    pub(super) fn record_target_after(&mut self, table: TableRef) -> Result<RecordTarget> {
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
    fn record_id(&mut self, at: Span) -> Result<Identity> {
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

impl Parser<'_> {
    /// `group::name(a, b)`, once the shape is already recognised.
    ///
    /// Arity is checked here rather than at evaluation because the set of
    /// functions is known when the statement is read, and a call with the wrong
    /// number of arguments is a mistake that never needs a record to see.
    fn call(&mut self, start: Span) -> Result<Expr> {
        // Read from the source rather than from the token, so a reserved word
        // used as a group keeps the case it was written in: `type::of` is a
        // function and `TYPE::of` is not, the same as every other name here.
        //
        // The half after `::` gets the same treatment, and for a reason that
        // arrived rather than being foreseen: `time::bucket` stopped parsing the
        // day files gave `BUCKET` a meaning. A function's name sits where
        // nothing but a name can stand, so a reserved word there is a name — and
        // reading it from the source keeps its case, which is what makes
        // `time::BUCKET` still not a function.
        let group = self.span_here();
        self.advance();
        self.expect_punct(Punct::ColonColon, "`::` after a function's group")?;
        let name = self.word_or_name()?;
        let Some(group) = self.source.get(group.start..group.end) else {
            return Err(self.error_here("a function's group"));
        };
        let spelling = format!("{group}::{}", name.text);
        let span = start.to(name.span);
        let Some(function) = Function::parse(&spelling) else {
            return Err(Error::NoSuchFunction {
                name: spelling,
                span,
            });
        };
        self.expect_punct(Punct::ParenOpen, "`(` after a function's name")?;
        let mut arguments = Vec::new();
        if !self.eat_punct(Punct::ParenClose) {
            arguments.push(self.expression()?);
            while self.eat_punct(Punct::Comma) {
                arguments.push(self.expression()?);
            }
            self.expect_punct(Punct::ParenClose, "`)` after the arguments")?;
        }
        if arguments.len() != function.arity() {
            return Err(Error::WrongArity {
                function,
                expected: function.arity(),
                found: arguments.len(),
                span,
            });
        }
        let whole = start.to(self.span_behind());
        Ok(Expr {
            kind: ExprKind::Call {
                function,
                arguments,
                span,
            },
            span: whole,
        })
    }
}
