//! Values, containers, and the two reads that may stand where a value stands.

use rust_decimal::Decimal;
use tessari_types::{Datetime, Number, RecordId, Value, parse_uuid};

use super::Parser;
use crate::ast::{
    Aggregate, Expr, ExprKind, Field, Identity, Name, RangeExpr, RecordTarget, TableRef,
};
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

    /// `IF <test> THEN <a> [ELSE IF <test> THEN <b>]* [ELSE <c>] END`
    ///
    /// `END` is required rather than optional, and closes the **whole** chain
    /// once. Without it `IF a THEN b ELSE c + 1` has two readings, and which one
    /// the grammar picked is not something a reader should have to know.
    ///
    /// The chain is read flat and built nested, so an `ELSE IF` is an ordinary
    /// [`ExprKind::If`] in the `otherwise` position and nothing downstream needs
    /// a second shape to walk.
    fn conditional(&mut self, start: Span) -> Result<Expr> {
        let mut arms = Vec::new();
        let mut otherwise = None;
        loop {
            self.advance();
            let condition = self.expression()?;
            self.expect_keyword(Keyword::Then, "`THEN` and the value it answers with")?;
            arms.push((condition, self.expression()?));
            if !self.eat_keyword(Keyword::Else) {
                break;
            }
            if self.peek_keyword() != Some(Keyword::If) {
                otherwise = Some(self.expression()?);
                break;
            }
        }
        self.expect_keyword(Keyword::End, "`END` to close the conditional")?;
        let span = start.to(self.span_behind());
        // Right to left, so the last arm holds the trailing `ELSE`.
        let mut built = otherwise;
        while let Some((condition, then)) = arms.pop() {
            built = Some(Expr {
                kind: ExprKind::If {
                    condition: Box::new(condition),
                    then: Box::new(then),
                    otherwise: built.map(Box::new),
                },
                span,
            });
        }
        // `arms` held at least one entry, so this is always `Some`.
        built.ok_or_else(|| self.error_here("a conditional"))
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
        if let Some(fold) = self.fold()? {
            return Ok(fold);
        }
        // Before `keyword_value`, which would read `IF` as a value it is not.
        // The two positions `IF` appears in never meet: here it leads an
        // expression, and in `DEFINE … IF NOT EXISTS` it follows a name.
        if self.peek_keyword() == Some(Keyword::If) {
            return self.conditional(span);
        }
        if let Some(keyword) = self.peek_keyword() {
            return self.keyword_value(keyword, span);
        }
        if self.ttl_follows() {
            return self.ttl_expression();
        }
        match self.peek() {
            // A shape is written the way RFC 7946 writes one, behind a marker:
            // `geometry { type: 'Point', coordinates: [2.35, 48.85] }`.
            //
            // The marker is a **contextual** word rather than a reserved one, so
            // `geometry` stays usable as a table name and as a field name —
            // reserving it would take a usable name away from data that already
            // exists. The brace is what makes it unambiguous: a table name is
            // never followed by an object.
            Some(Token::Ident(word))
                if word.eq_ignore_ascii_case("geometry")
                    && self.follows_with(1, &Token::Punct(Punct::BraceOpen)) =>
            {
                self.geometry_literal(span)
            }
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

    /// `geometry { type: 'Point', coordinates: [2.35, 48.85] }`.
    ///
    /// The object is read by the ordinary object parser, so nesting, commas and
    /// trailing-comma behaviour are the language's and not a second dialect.
    /// What is added on top is two refusals:
    ///
    /// - every part must be **written out**. A shape literal is read at parse
    ///   time, so a field, a parameter or a call inside one would have to be
    ///   evaluated — and a shape that could differ per record is not a literal.
    ///   Such a shape is written with a bound parameter instead, which is a
    ///   complete path and is what a client uses.
    /// - the object must actually describe a shape, judged by the same reader
    ///   the HTTP surface uses, so the refusal a caller reads is the same
    ///   sentence whichever door they came through.
    ///
    /// Validity — closed rings, holes inside their shell — is **not** judged
    /// here. It is judged when the shape reaches a record, after snapping, and
    /// judging it twice in two places would eventually be judging it differently.
    fn geometry_literal(&mut self, span: Span) -> Result<Expr> {
        self.advance();
        let open = self.span_here();
        let object = self.object(open)?;
        let whole = span.to(object.span);
        let written = constant(&object).ok_or(Error::ComputedGeometry {
            found: "a value that has to be computed",
            span: whole,
        })?;
        let shape = tessari_types::from_geojson(&written).map_err(|malformed| {
            Error::MalformedGeometry {
                reason: malformed.to_string(),
                span: whole,
            }
        })?;
        Ok(Expr {
            kind: ExprKind::Literal(Value::Geometry(shape)),
            span: whole,
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

    pub(super) fn quoted_field_name(&mut self) -> Result<(String, Span)> {
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
    pub(super) fn record_id(&mut self, at: Span) -> Result<Identity> {
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
    /// `count(*)`, `mean(age)` — a fold, when one stands here.
    ///
    /// Recognised by the word and the `(` after it, so a field called `count` is
    /// still readable everywhere a field can stand: only `count(` is a fold, and
    /// a bare `count` is a route into the record. That is the same rule the six
    /// contextual words of `ORDER BY` follow, and for the same reason.
    ///
    /// Read here rather than in the projection, which is what makes
    /// `mean(price) * 1.2` writable: a fold is an expression, so everything an
    /// expression can be part of, a fold can be part of. Where a fold may
    /// *stand* is a separate question, answered when the statement's shape is
    /// checked — a condition refuses one, and says that a filter over groups is
    /// `HAVING`.
    fn fold(&mut self) -> Result<Option<Expr>> {
        let start = self.span_here();
        let Some(Token::Ident(word)) = self.peek() else {
            return Ok(None);
        };
        let Some(fold) = Aggregate::parse(word) else {
            return Ok(None);
        };
        if !self.follows_with(1, &Token::Punct(Punct::ParenOpen)) {
            return Ok(None);
        }
        self.advance();
        self.advance();
        // `count(*)` folds over the records themselves; every other fold, and
        // `count(<expr>)`, folds over a value in each of them.
        let over = if self.eat_punct(Punct::Star) {
            None
        } else {
            Some(Box::new(self.condition()?))
        };
        let end = self.expect_punct(Punct::ParenClose, "`)` after what is folded")?;
        if over.is_none() && fold != Aggregate::Count {
            return Err(Error::StarIsOnlyForCount {
                fold: fold.spelling(),
                span: start.to(end),
            });
        }
        let span = start.to(end);
        Ok(Some(Expr {
            kind: ExprKind::Fold { fold, over, span },
            span,
        }))
    }

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

/// The value an expression already is, when every part of it is written out.
///
/// `None` for anything that would have to be evaluated. Used only by the shape
/// literal, which is read at parse time and therefore cannot wait for a record.
fn constant(expr: &Expr) -> Option<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Some(value.clone()),
        ExprKind::Array(items) => items
            .iter()
            .map(constant)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        ExprKind::Object(fields) => fields
            .iter()
            .map(|field| constant(&field.value).map(|value| (field.name.text.clone(), value)))
            .collect::<Option<std::collections::BTreeMap<_, _>>>()
            .map(Value::Object),
        // A written-out negative number reaches the parser as a negation of a
        // positive one, and a coordinate west of Greenwich is exactly that.
        ExprKind::Negate(inner) => match constant(inner)? {
            Value::Number(Number::Integer(whole)) => {
                Some(Value::Number(Number::Integer(whole.saturating_neg())))
            }
            Value::Number(Number::Float(held)) => Some(Value::Number(Number::float(-held))),
            _ => None,
        },
        _ => None,
    }
}
