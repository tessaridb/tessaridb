//! Values, containers, and the two reads that may stand where a value stands.

mod calls;
mod literals;
mod references;
use tessari_types::{Number, Value};

use super::Parser;
use crate::ast::{Expr, ExprKind, RangeExpr, RecordTarget};
use crate::error::Result;
use crate::token::{Keyword, Punct, Span, Token};

impl Parser<'_> {
    /// A value or a test, at the loosest binding.
    ///
    /// Precedence, loosest first: `OR`, `AND`, `NOT`, then the comparisons —
    /// which are **non-associative**, so `a < b < c` is refused rather than read
    /// as one of the two things it might mean — then a range, then a primary.
    /// Parentheses override, as everywhere.
    pub(super) fn expression(&mut self) -> Result<Expr> {
        self.nested(Self::disjunction)
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
                let parameter = Expr { kind, span };
                self.route_into(parameter)
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

    /// The string a marker keyword applies to.
    fn marked_string(&mut self, expected: &'static str) -> Result<(String, Span)> {
        // Past the marker — `datetime`, `uuid` — which the caller has peeked at
        // and not consumed.
        self.advance();
        self.text(expected)
    }

    pub(super) fn record_target(&mut self) -> Result<RecordTarget> {
        let table = self.table_ref()?;
        self.record_target_after(table)
    }
}

impl Parser<'_> {}

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

impl Parser<'_> {
    /// The steps written after a value, `.field` and `[n]`, as a route into it
    /// — or the value itself when none follow.
    fn route_into(&mut self, value: Expr) -> Result<Expr> {
        let mut steps = Vec::new();
        loop {
            if self.eat_punct(Punct::Dot) {
                steps.push(tessari_types::Step::Field(self.name()?.text));
            } else if self.peek() == Some(&Token::Punct(Punct::BracketOpen))
                && matches!(
                    self.tokens
                        .get(self.position.saturating_add(1))
                        .map(|spanned| &spanned.token),
                    Some(Token::Number(_))
                )
            {
                self.advance();
                steps.push(tessari_types::Step::Index(self.array_position()?));
                self.expect_punct(Punct::BracketClose, "`]` after a position")?;
            } else {
                break;
            }
        }
        if steps.is_empty() {
            return Ok(value);
        }
        let span = value.span.to(self.span_behind());
        Ok(Expr {
            kind: ExprKind::Route {
                value: Box::new(value),
                steps,
            },
            span,
        })
    }
}
