//! Reading tokens into the abstract syntax.
//!
//! Recursive descent, one function per grammatical form, no backtracking. The
//! grammar is small enough that every decision is made on one or two tokens of
//! lookahead, and keeping it that way is a constraint worth defending: a
//! grammar that needs backtracking is a grammar whose error messages point at
//! the wrong place.
//!
//! The parser holds the **source text** as well as the tokens, for one reason.
//! An exact decimal is written `dec 12.34`, and the lexer has already turned
//! `12.34` into a float. Reading the decimal back out of that float would round
//! it — which is precisely what the marker exists to prevent — so the decimal is
//! read from the characters the author wrote.

mod expression;
mod path;
mod statement;

use crate::ast::Script;
use crate::error::{Error, Result};
use crate::lexer::tokenize;
use crate::token::{Keyword, Punct, Span, Spanned, Token};

/// Read `source` into a script.
///
/// # Errors
///
/// Returns the first failure — lexical or grammatical — with the span it
/// occurred at.
pub fn parse(source: &str) -> Result<Script> {
    let tokens = tokenize(source)?;
    Parser {
        source,
        tokens,
        position: 0,
    }
    .script()
}

/// A cursor over the tokens, and the source they came from.
struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Spanned>,
    position: usize,
}

impl Parser<'_> {
    fn script(mut self) -> Result<Script> {
        let mut statements = Vec::new();
        while self.peek().is_some() {
            statements.push(self.statement()?);
            // A trailing `;` is optional; a missing one between statements is
            // not, because two statements running together parse as neither.
            if !self.eat_punct(Punct::Semicolon) && self.peek().is_some() {
                return Err(self.error_here("`;` between statements"));
            }
        }
        Ok(Script {
            statements,
            span: Span::new(0, self.source.len()),
        })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position).map(|spanned| &spanned.token)
    }

    /// The keyword under the cursor, if there is one.
    fn peek_keyword(&self) -> Option<Keyword> {
        match self.peek() {
            Some(Token::Keyword(keyword)) => Some(*keyword),
            _ => None,
        }
    }

    /// Where the cursor stands, or the end of the source.
    fn span_here(&self) -> Span {
        self.tokens
            .get(self.position)
            .map_or_else(|| self.end_of_source(), |spanned| spanned.span)
    }

    /// Where the last consumed token ended.
    fn span_behind(&self) -> Span {
        self.position
            .checked_sub(1)
            .and_then(|previous| self.tokens.get(previous))
            .map_or_else(|| self.end_of_source(), |spanned| spanned.span)
    }

    fn end_of_source(&self) -> Span {
        Span::new(self.source.len(), self.source.len())
    }

    fn advance(&mut self) -> Option<Spanned> {
        let spanned = self.tokens.get(self.position).cloned()?;
        self.position = self.position.saturating_add(1);
        Some(spanned)
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        if self.peek_keyword() == Some(keyword) {
            self.position = self.position.saturating_add(1);
            return true;
        }
        false
    }

    fn eat_punct(&mut self, punct: Punct) -> bool {
        if self.peek() == Some(&Token::Punct(punct)) {
            self.position = self.position.saturating_add(1);
            return true;
        }
        false
    }

    fn expect_keyword(&mut self, keyword: Keyword, expected: &'static str) -> Result<Span> {
        if self.eat_keyword(keyword) {
            return Ok(self.span_behind());
        }
        Err(self.error_here(expected))
    }

    fn expect_punct(&mut self, punct: Punct, expected: &'static str) -> Result<Span> {
        if self.eat_punct(punct) {
            return Ok(self.span_behind());
        }
        Err(self.error_here(expected))
    }

    /// `IF NOT EXISTS`, when it is there.
    ///
    /// Written before the name, as every other statement that takes it does.
    fn eat_if_not_exists(&mut self) -> Result<bool> {
        if !self.eat_keyword(Keyword::If) {
            return Ok(false);
        }
        self.expect_keyword(Keyword::Not, "`NOT` after `IF`")?;
        self.expect_keyword(Keyword::Exists, "`EXISTS` after `IF NOT`")?;
        Ok(true)
    }

    /// The failure for whatever stands under the cursor.
    fn error_here(&self, expected: &'static str) -> Error {
        let Some(spanned) = self.tokens.get(self.position) else {
            return Error::UnexpectedEnd {
                expected,
                span: self.end_of_source(),
            };
        };
        if let Token::Ident(word) = &spanned.token {
            if let Some(feature) = absent_feature(word) {
                return Error::Unsupported {
                    feature,
                    span: spanned.span,
                };
            }
        }
        Error::UnexpectedToken {
            found: describe(&spanned.token),
            expected,
            span: spanned.span,
        }
    }
}

/// The words that name something `docs/bgvql.md` §8 leaves out on purpose.
///
/// They are looked up here rather than reserved as keywords, so that a table
/// called `order` stays legal while `ORDER BY` still gets an answer that names
/// the reason instead of complaining about a semicolon.
fn absent_feature(word: &str) -> Option<&'static str> {
    const ABSENT: &[(&str, &str)] = &[
        ("join", "joins"),
        ("inner", "joins"),
        ("left", "joins"),
        ("group", "aggregation and grouping"),
        ("having", "aggregation and grouping"),
        ("count", "aggregation and grouping"),
        ("order", "ordering a result"),
        ("limit", "limiting a result"),
        ("offset", "limiting a result"),
        ("match", "full-text search"),
        ("search", "full-text search"),
        ("knn", "vector search"),
        ("grant", "permissions in the language"),
        ("revoke", "permissions in the language"),
        ("permissions", "permissions in the language"),
    ];
    ABSENT
        .iter()
        .find(|(spelling, _)| spelling.eq_ignore_ascii_case(word))
        .map(|(_, feature)| *feature)
}

/// How a token is named back to whoever wrote it.
fn describe(token: &Token) -> String {
    match token {
        Token::Keyword(keyword) => format!("`{keyword}`"),
        Token::Ident(name) => format!("the name `{name}`"),
        Token::Number(_) => "a number".to_owned(),
        Token::Str(_) => "text".to_owned(),
        Token::Bytes(_) => "bytes".to_owned(),
        Token::Duration(_) => "a duration".to_owned(),
        Token::Punct(punct) => format!("`{punct}`"),
    }
}
