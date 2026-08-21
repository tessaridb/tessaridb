//! Reading a name, and reading a route into a record.
//!
//! One file because they are the same decision at two depths: a plain field name
//! is a route of one step, and the only thing that separates them is whether a
//! delimiter follows. Keeping them apart would have put the rule for what may
//! stand in a name in two places.

use bgv_db_types::{Number, Path, Step};

use super::Parser;
use crate::ast::{FieldPath, Name};
use crate::error::Result;
use crate::token::{Punct, Spanned, Token};

impl Parser<'_> {
    /// A route to a value inside a record: `email`, `address.city`, `tags[0]`.
    ///
    /// Read only where a *value inside a record* is meant — a filter's left side
    /// and an index's projection. It is never read where a table may stand,
    /// because `.` already qualifies a table by its database and `orders.users`
    /// would otherwise start reading as a path into a table called `orders`.
    /// The parser tests assert that separation rather than the grammar being
    /// trusted to keep it.
    pub(super) fn field_path(&mut self) -> Result<FieldPath> {
        let root = self.name()?;
        let start = root.span;
        let mut steps = Vec::new();
        loop {
            if self.eat_punct(Punct::Dot) {
                steps.push(Step::Field(self.name()?.text));
            } else if self.eat_punct(Punct::BracketOpen) {
                steps.push(Step::Index(self.array_position()?));
                self.expect_punct(Punct::BracketClose, "`]` after a position")?;
            } else {
                break;
            }
        }
        Ok(FieldPath {
            path: Path::new(root.text, steps),
            span: start.to(self.span_behind()),
        })
    }

    /// A position inside an array: a whole number, never negative.
    ///
    /// Counting from the end would need a sign the storage layer has no way to
    /// resolve without knowing the array's length, which is a decision about
    /// what a path *means* rather than how one is written.
    fn array_position(&mut self) -> Result<u64> {
        let expected = "a position, as a whole number";
        let Some(Token::Number(Number::Integer(at))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let at = u64::try_from(*at).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(at)
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
