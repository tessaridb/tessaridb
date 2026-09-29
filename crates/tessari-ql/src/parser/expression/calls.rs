//! Function calls and folds.

use super::super::Parser;
use crate::ast::{Aggregate, Expr, ExprKind};
use crate::error::{Error, Result};
use crate::function::Function;
use crate::token::{Punct, Span, Token};

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
    pub(crate) fn fold(&mut self) -> Result<Option<Expr>> {
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
        // The counter folds order their values by an instant, written second.
        let at = if fold.takes_an_instant() {
            self.expect_punct(
                Punct::Comma,
                "`,` and the instant each value was observed at",
            )?;
            Some(Box::new(self.condition()?))
        } else {
            None
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
            kind: ExprKind::Fold {
                fold,
                over,
                at,
                span,
            },
            span,
        }))
    }

    pub(crate) fn call(&mut self, start: Span) -> Result<Expr> {
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
