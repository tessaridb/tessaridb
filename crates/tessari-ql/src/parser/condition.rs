//! How a condition is built out of values.
//!
//! The precedence ladder, and nothing else. Separate from the values it composes
//! because the two answer different questions — *what is a value* and *how do
//! tests combine* — and because a grammar's precedence is the part a reader
//! comes looking for.
//!
//! Loosest to tightest: `OR`, `AND`, `NOT`, the comparisons, `+` and `-`, then
//! `*` `/` `%`, then a unary minus, then a range, then a primary. Parentheses
//! override, as everywhere.

use super::Parser;
use crate::ast::{ArithmeticOp, BinaryOp, Expr, ExprKind, Written};
use crate::error::Result;
use crate::token::{Keyword, Punct};

impl Parser<'_> {
    /// A condition: an expression in which a bare name reads as a path.
    ///
    /// The whole of the difference between the two positions. In a value
    /// position `users` is a table, because `CREATE audit:1 = { subject: users }`
    /// means the table; in a condition it is a route into the record being
    /// tested, because `WHERE name = 'ada'` means the field. One token, two
    /// readings, decided by where it stands and nowhere else.
    pub(super) fn condition(&mut self) -> Result<Expr> {
        let outer = self.reading_paths;
        self.reading_paths = true;
        let parsed = self.expression();
        self.reading_paths = outer;
        parsed
    }

    /// An expression, kept as the text it was written as.
    ///
    /// Parsed so that it is known to be one, and then taken from the source, so
    /// what the catalog stores is what the author typed.
    pub(super) fn written_expression(&mut self) -> Result<Written> {
        let parsed = self.expression()?;
        let text = self
            .source
            .get(parsed.span.start..parsed.span.end)
            .unwrap_or_default()
            .to_owned();
        Ok(Written {
            text,
            span: parsed.span,
        })
    }

    pub(super) fn disjunction(&mut self) -> Result<Expr> {
        let mut left = self.conjunction()?;
        while self.eat_keyword(Keyword::Or) {
            let right = self.conjunction()?;
            let span = left.span.to(right.span);
            left = Expr {
                kind: ExprKind::Or(Box::new(left), Box::new(right)),
                span,
            };
        }
        Ok(left)
    }

    fn conjunction(&mut self) -> Result<Expr> {
        let mut left = self.negation()?;
        while self.eat_keyword(Keyword::And) {
            let right = self.negation()?;
            let span = left.span.to(right.span);
            left = Expr {
                kind: ExprKind::And(Box::new(left), Box::new(right)),
                span,
            };
        }
        Ok(left)
    }

    fn negation(&mut self) -> Result<Expr> {
        let start = self.span_here();
        if !self.eat_keyword(Keyword::Not) {
            return self.comparison();
        }
        let operand = self.negation()?;
        let span = start.to(operand.span);
        Ok(Expr {
            kind: ExprKind::Not(Box::new(operand)),
            span,
        })
    }

    /// One comparison, and never two in a row.
    ///
    /// `a < b < c` is refused because both readings — `(a < b) < c` and "b is
    /// between a and c" — are things somebody means, and a grammar that silently
    /// picks one answers a question that was not asked.
    fn comparison(&mut self) -> Result<Expr> {
        let left = self.additive()?;
        let Some(op) = self.comparison_operator() else {
            return Ok(left);
        };
        let right = self.additive()?;
        if self.comparison_operator().is_some() {
            return Err(self.error_here("`AND`, `OR` or the end of the condition"));
        }
        Ok(binary(op, left, right))
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = if self.eat_punct(Punct::Plus) {
                ArithmeticOp::Add
            } else if self.eat_punct(Punct::Minus) {
                ArithmeticOp::Subtract
            } else {
                return Ok(left);
            };
            left = arithmetic(op, left, self.multiplicative()?);
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.negative()?;
        loop {
            let op = if self.eat_punct(Punct::Star) {
                ArithmeticOp::Multiply
            } else if self.eat_punct(Punct::Slash) {
                ArithmeticOp::Divide
            } else if self.eat_punct(Punct::Percent) {
                ArithmeticOp::Remainder
            } else {
                return Ok(left);
            };
            left = arithmetic(op, left, self.negative()?);
        }
    }

    /// A negation of something that is not a literal.
    ///
    /// `-7` never reaches here: the lexer reads a minus touching digits as part
    /// of the number, so a negative literal is one token. What this handles is
    /// `-price` and `-(a + b)`.
    fn negative(&mut self) -> Result<Expr> {
        let start = self.span_here();
        if !self.eat_punct(Punct::Minus) {
            return self.spanned_range();
        }
        let operand = self.negative()?;
        let span = start.to(operand.span);
        Ok(Expr {
            kind: ExprKind::Negate(Box::new(operand)),
            span,
        })
    }

    fn comparison_operator(&mut self) -> Option<BinaryOp> {
        for (punct, op) in [
            (Punct::Equals, BinaryOp::Equal),
            (Punct::NotEquals, BinaryOp::NotEqual),
            (Punct::LessOrEqual, BinaryOp::LessOrEqual),
            (Punct::Less, BinaryOp::Less),
            (Punct::GreaterOrEqual, BinaryOp::GreaterOrEqual),
            (Punct::Greater, BinaryOp::Greater),
        ] {
            if self.eat_punct(punct) {
                return Some(op);
            }
        }
        for (keyword, op) in [
            (Keyword::In, BinaryOp::In),
            (Keyword::Contains, BinaryOp::Contains),
            (Keyword::Like, BinaryOp::Like),
            (Keyword::Ilike, BinaryOp::Ilike),
            (Keyword::Matches, BinaryOp::Matches),
        ] {
            if self.eat_keyword(keyword) {
                return Some(op);
            }
        }
        None
    }
}

/// Two operands joined, spanning both.
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    let span = left.span.to(right.span);
    Expr {
        kind: ExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        span,
    }
}

/// Two numbers joined, spanning both.
fn arithmetic(op: ArithmeticOp, left: Expr, right: Expr) -> Expr {
    let span = left.span.to(right.span);
    Expr {
        kind: ExprKind::Arithmetic {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        span,
    }
}
