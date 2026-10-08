//! `DEFINE PARAM` and `DROP PARAM` (ADR-0124 D2).

use crate::ast::StatementKind;
use crate::error::Result;
use crate::parser::Parser;
use crate::token::{Span, Token};

impl Parser<'_> {
    /// `DEFINE PARAM [IF NOT EXISTS | OR REPLACE] $name VALUE <expr>`, the words
    /// through `PARAM` consumed.
    pub(crate) fn define_param(&mut self) -> Result<StatementKind> {
        let (if_not_exists, or_replace) = self.eat_definition_mode()?;
        let (name, span) = self.param_name()?;
        self.expect_word("value", "`VALUE` and what the param holds")?;
        Ok(StatementKind::DefineParam {
            name,
            value: self.expression()?,
            if_not_exists,
            or_replace,
            span,
        })
    }

    /// A param's name, written with its marker: `$grace`.
    pub(crate) fn param_name(&mut self) -> Result<(String, Span)> {
        let Some(Token::Parameter(name)) = self.peek() else {
            return Err(self.error_here("a param's name, written with its `$`"));
        };
        let name = name.clone();
        let span = self.span_here();
        self.advance();
        Ok((name, span))
    }
}
