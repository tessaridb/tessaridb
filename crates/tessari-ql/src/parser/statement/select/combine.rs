//! `UNION [ALL]`, `INTERSECT` and `EXCEPT` between reads (ADR-0134 D6).

use super::super::Parser;

use crate::ast::{Combination, Select, SetOperation};
use crate::error::{Error, Result};
use crate::token::Keyword;

impl Parser<'_> {
    /// A read and every read it combines with, left to right.
    ///
    /// Each operand is a whole read with its own clauses, so `a UNION b LIMIT 5`
    /// bounds `b`; a bound on the combined answer is written by reading it as a
    /// source.
    pub(in crate::parser) fn select_statement(&mut self) -> Result<Select> {
        let mut first = self.select_single()?;
        loop {
            let span = self.span_here();
            let operation = if self.eat_word("union") {
                SetOperation::Union
            } else if self.eat_word("intersect") {
                SetOperation::Intersect
            } else if self.eat_word("except") {
                SetOperation::Except
            } else {
                return Ok(first);
            };
            let all = self.eat_word("all");
            if all && operation != SetOperation::Union {
                return Err(Error::UnexpectedToken {
                    expected: "`SELECT` — `ALL` keeps duplicates, and only `UNION` makes them",
                    found: "`ALL`".to_owned(),
                    span: self.span_behind(),
                });
            }
            if self.peek_keyword() != Some(Keyword::Select) {
                return Err(self.error_here("`SELECT` and the read to combine with"));
            }
            let read = self.select_single()?;
            first.combined.push(Combination {
                operation,
                all,
                read: Box::new(read),
                span,
            });
        }
    }
}
