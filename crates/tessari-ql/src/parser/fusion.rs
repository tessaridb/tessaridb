//! `ORDER BY FUSE (…) [DEPTH n]` — several orderings fused by rank (G038).
//!
//! # Contextual, like every other order word
//!
//! `fuse`, `weight` and `depth` stay field names: `ORDER BY fuse` sorts by a
//! field called `fuse`, and only `FUSE` followed by `(` opens a fused order —
//! a bare field is never followed by a parenthesis in that position.

use tessari_types::Number;

use super::Parser;
use crate::ast::{Fusion, Ordering};
use crate::error::{Error, Result};
use crate::token::{Punct, Token};

impl Parser<'_> {
    /// `FUSE (key [ASC|DESC] [WEIGHT w], …) [DEPTH n]`, with `ORDER BY` consumed.
    ///
    /// `None` when what stands here is an ordinary order. Each branch is read as
    /// a sort key is, so a branch is exactly what the same words would mean
    /// after `ORDER BY` alone.
    pub(super) fn fused_order(&mut self) -> Result<Option<(Vec<Ordering>, Fusion)>> {
        if !(self.peek_word("fuse") && self.follows_with(1, &Token::Punct(Punct::ParenOpen))) {
            return Ok(None);
        }
        let start = self.span_here();
        self.eat_word("fuse");
        self.advance();
        let mut branches = Vec::new();
        let mut weights = Vec::new();
        loop {
            branches.push(self.ordering()?);
            weights.push(if self.eat_word("weight") {
                self.positive_weight()?
            } else {
                Number::Integer(1)
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::ParenClose, "`)` after the fused orders")?;
        let span = start.to(self.span_behind());
        // One branch has nothing to fuse with: its fused order is its own order,
        // and a reader of the statement would be looking for the second one.
        if branches.len() < 2 {
            return Err(Error::UnexpectedToken {
                expected: "at least two orders to fuse — one order is written without `FUSE`",
                found: "one".to_owned(),
                span,
            });
        }
        let depth = if self.eat_word("depth") {
            Some(self.positive_count("a whole number above zero — how far down each order counts")?)
        } else {
            None
        };
        // A fused order is the whole order: a second key would have to break the
        // fused ties, which identity already does, or be fused itself.
        if self.peek() == Some(&Token::Punct(Punct::Comma)) {
            return Err(self.error_here(
                "the end of the order — a fused order is the whole order, so another key \
                 belongs inside `FUSE (…)`",
            ));
        }
        Ok(Some((
            branches,
            Fusion {
                weights,
                depth,
                span,
            },
        )))
    }

    /// A branch's weight: a number above zero.
    fn positive_weight(&mut self) -> Result<Number> {
        let expected = "a number above zero — how much this order counts";
        let weight = match self.peek() {
            Some(Token::Number(Number::Integer(n))) if *n > 0 => Number::Integer(*n),
            Some(Token::Number(Number::Float(n))) if *n > 0.0 && n.is_finite() => Number::float(*n),
            _ => return Err(self.error_here(expected)),
        };
        self.advance();
        Ok(weight)
    }
}
