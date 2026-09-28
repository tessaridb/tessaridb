//! Literal values: numbers, decimals, times, uuids, text, keywords and shapes.

use super::super::Parser;
use super::constant;
use crate::ast::{Expr, ExprKind};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Span, Spanned, Token};
use rust_decimal::Decimal;
use tessari_types::{Datetime, Number, Value, parse_uuid};

impl Parser<'_> {
    /// The values a keyword introduces, and the reads that are also values.
    pub(crate) fn keyword_value(&mut self, keyword: Keyword, span: Span) -> Result<Expr> {
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
    pub(crate) fn decimal(&mut self, span: Span) -> Result<Expr> {
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

    pub(crate) fn datetime(&mut self, span: Span) -> Result<Expr> {
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

    pub(crate) fn uuid(&mut self, span: Span) -> Result<Expr> {
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

    /// A string literal standing where one is required.
    pub(crate) fn text(&mut self, expected: &'static str) -> Result<(String, Span)> {
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
    pub(crate) fn literal_token(&mut self, span: Span) -> Result<Expr> {
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
                    found: super::super::describe(&spanned.token),
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
    pub(crate) fn geometry_literal(&mut self, span: Span) -> Result<Expr> {
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
}
