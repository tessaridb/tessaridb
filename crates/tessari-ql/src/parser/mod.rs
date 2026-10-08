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

mod assertion;
mod condition;
mod expression;
mod fusion;
mod group;
mod key_value;
mod path;
mod shape;
mod statement;
mod topic;

use std::collections::BTreeSet;

use crate::ast::{Expr, Script, Select, Statement, StatementKind};
use crate::error::{Error, Result};
use crate::lexer::tokenize;
use crate::token::{Keyword, Punct, Span, Spanned, Token};

/// What a script may say about names it binds and the value it answers with.
///
/// Both rules are properties of the statement **text**, so they are settled
/// here rather than left for something to discover at run time: a script that
/// binds a name twice or answers twice is wrong whether or not it is ever run,
/// and refusing it before anything runs means it cannot half-run first.
fn check_bindings(statements: &[Statement]) -> Result<()> {
    let mut bound: BTreeSet<&str> = BTreeSet::new();
    let mut answered = false;
    for statement in statements {
        match &statement.kind {
            StatementKind::Let { name, span, .. } => {
                if !bound.insert(name.as_str()) {
                    return Err(Error::BoundTwice {
                        name: name.clone(),
                        span: *span,
                    });
                }
            }
            StatementKind::Return { .. } => {
                if answered {
                    return Err(Error::ReturnedTwice {
                        span: statement.span,
                    });
                }
                answered = true;
            }
            _ => {}
        }
    }
    Ok(())
}

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
        reading_paths: false,
        depth: 0,
        dropping_if_exists: false,
    }
    .script()
}

/// Read `source` into one expression, with nothing around it.
///
/// The way a default stored in the catalog is read back. A definition keeps the
/// text it was written as (the same choice a field kind and a path already
/// make), so something has to turn that text into an expression again, and the
/// crate that can parse is this one.
///
/// # Errors
///
/// Returns the first failure, and refuses text that parses as an expression
/// followed by anything else — a stored default is one expression or it is not
/// a default.
pub fn parse_expression(source: &str) -> Result<Expr> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        source,
        tokens,
        position: 0,
        reading_paths: false,
        depth: 0,
        dropping_if_exists: false,
    };
    let expression = parser.expression()?;
    if parser.peek().is_some() {
        return Err(parser.error_here("the end of the expression"));
    }
    Ok(expression)
}

/// Read `source` as one condition, the way a `WHERE` reads one: a bare name is
/// a field of the record being tested.
///
/// For a condition that arrives on its own — a subscription's (ADR-0122 Part
/// B) — where [`parse_expression`] would read `chat` as a table.
///
/// # Errors
///
/// Returns the first failure, and refuses a condition followed by anything
/// else.
pub fn parse_condition(source: &str) -> Result<Expr> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        source,
        tokens,
        position: 0,
        reading_paths: false,
        depth: 0,
        dropping_if_exists: false,
    };
    let condition = parser.condition()?;
    if parser.peek().is_some() {
        return Err(parser.error_here("the end of the condition"));
    }
    Ok(condition)
}

/// Read `source` into one read, with nothing around it.
///
/// The way a view stored in the catalog is read back — the same shape
/// [`parse_expression`] has, for the same reason: a definition keeps the text it
/// was written as, so something has to turn that text into a tree again.
///
/// # Errors
///
/// Returns the first failure, refuses text that does not begin with `SELECT`,
/// and refuses a read followed by anything else — a stored view is one read or
/// it is not a view.
pub fn parse_read(source: &str) -> Result<Select> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        source,
        tokens,
        position: 0,
        reading_paths: false,
        depth: 0,
        dropping_if_exists: false,
    };
    if parser.peek_keyword() != Some(Keyword::Select) {
        return Err(parser.error_here("`SELECT` — a view is a read"));
    }
    let read = parser.select_statement()?;
    if parser.peek().is_some() {
        return Err(parser.error_here("the end of the read"));
    }
    Ok(read)
}

/// A cursor over the tokens, and the source they came from.
struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Spanned>,
    position: usize,
    /// Whether a bare name here reads as a route into a record.
    ///
    /// True inside a condition and false everywhere else, because `users` means
    /// the table in `CREATE audit:1 = { subject: users }` and the field in
    /// `WHERE users = 3`. Held on the parser rather than threaded through every
    /// expression rule, since every rule between the condition and the name
    /// would otherwise carry a parameter it does not use.
    reading_paths: bool,
    /// How many expressions deep the reader is, so a statement nested past
    /// [`MAX_EXPRESSION_DEPTH`] is refused instead of exhausting the stack.
    depth: usize,
    /// Whether the `DROP` being read said `IF EXISTS` (ADR-0124 D1).
    dropping_if_exists: bool,
}

/// How many expressions deep a statement may nest.
///
/// The value ceiling, and derived from the stack rather than from it: one
/// level of a container literal costs about 27 KB of stack in a debug build and
/// 9 KB in release (measured 2026-09-27), and a parsing thread gets the 2 MiB
/// every spawned thread gets. So a literal reaches one level short of the
/// deepest storable value, and a value that deep is sent as a parameter. A test
/// holds the ceiling inside that stack.
const MAX_EXPRESSION_DEPTH: usize = tessari_types::MAX_NESTING;

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
        check_bindings(&statements)?;
        Ok(Script {
            statements,
            span: Span::new(0, self.source.len()),
        })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position).map(|spanned| &spanned.token)
    }

    /// The token `offset` places past the cursor, when there is one.
    fn peek_ahead(&self, offset: usize) -> Option<&Token> {
        self.tokens
            .get(self.position.saturating_add(offset))
            .map(|spanned| &spanned.token)
    }

    /// Whether the token `offset` places past the cursor is this one.
    /// Whether the token `offset` ahead is the contextual word `word`.
    fn follows_word(&self, offset: usize, word: &str) -> bool {
        matches!(
            self.tokens
                .get(self.position.saturating_add(offset))
                .map(|spanned| &spanned.token),
            Some(Token::Ident(found)) if found.eq_ignore_ascii_case(word)
        )
    }

    fn follows_with(&self, offset: usize, token: &Token) -> bool {
        self.tokens
            .get(self.position.saturating_add(offset))
            .is_some_and(|spanned| &spanned.token == token)
    }

    /// Consume a **contextual** word: one that shapes a clause without being
    /// reserved.
    ///
    /// `ORDER`, `BY`, `LIMIT`, `START`, `ASC` and `DESC` are read this way
    /// rather than added to [`Keyword`], because reserving them would take four
    /// perfectly good field and table names away from data that already exists —
    /// and this language has a rule about that: an absent feature is *looked up*
    /// to give a better error, never reserved. Nothing else can stand in the
    /// positions these appear in, so nothing is ambiguous.
    ///
    /// Matched case-insensitively, like a keyword, because that is what it is
    /// everywhere except in the token table.
    /// Whether the next token is this contextual word, without consuming it.
    ///
    /// For a clause that has something to refuse *before* it starts reading, so
    /// that the refusal points at the clause word rather than at whatever
    /// followed it.
    fn peek_word(&self, word: &str) -> bool {
        matches!(
            self.peek(),
            Some(Token::Ident(found)) if found.eq_ignore_ascii_case(word)
        )
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let matched = matches!(
            self.peek(),
            Some(Token::Ident(found)) if found.eq_ignore_ascii_case(word)
        );
        if matched {
            self.position = self.position.saturating_add(1);
        }
        matched
    }

    /// Whether what stands here is a call: a name, `::`, a name, then `(`.
    ///
    /// Three tokens of lookahead rather than one, which is the only place this
    /// grammar needs more than two — and it is worth it, because the
    /// alternative is a function namespace that a field could shadow.
    fn call_follows(&self) -> bool {
        let at = |offset: usize| {
            self.tokens
                .get(self.position.saturating_add(offset))
                .map(|spanned| &spanned.token)
        };
        // A reserved word is admitted on **both** sides of the `::`. The group
        // always was — `type::of` is a function. The name half was added when
        // `time::bucket` stopped parsing the day files gave `BUCKET` a meaning:
        // nothing but a name can stand there, so a reserved word there is a
        // name, and the alternative is a grammar that quietly loses a function
        // every time a word is reserved somewhere else.
        matches!(at(0), Some(Token::Ident(_) | Token::Keyword(_)))
            && matches!(at(1), Some(Token::Punct(Punct::ColonColon)))
            && matches!(at(2), Some(Token::Ident(_) | Token::Keyword(_)))
            && matches!(at(3), Some(Token::Punct(Punct::ParenOpen)))
    }

    /// Whether what stands here is a record reference rather than a route.
    ///
    /// `users:1` is a record in either position; `users` and `users.name` are a
    /// route in a condition. One token of lookahead past the name decides it,
    /// which keeps the grammar backtrack-free.
    fn record_follows(&self) -> bool {
        matches!(
            self.tokens
                .get(self.position.saturating_add(1))
                .map(|s| &s.token),
            Some(Token::Punct(Punct::Colon))
        )
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

    /// Whether this punctuation stands at the cursor, without consuming it.
    fn at_punct(&self, punct: Punct) -> bool {
        self.peek() == Some(&Token::Punct(punct))
    }

    fn eat_punct(&mut self, punct: Punct) -> bool {
        if self.at_punct(punct) {
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
    /// `IF NOT EXISTS` or `OR REPLACE`, whichever stands here: keep what is
    /// there, or replace it (ADR-0124 D1). Both together are refused, because
    /// keeping and replacing cannot both be meant.
    fn eat_definition_mode(&mut self) -> Result<(bool, bool)> {
        let keep = self.eat_if_not_exists()?;
        let replace = self.peek_keyword() == Some(Keyword::Or);
        if replace {
            if keep {
                return Err(self.error_here(
                    "the name — `IF NOT EXISTS` keeps what is there and `OR REPLACE` \
                     replaces it, so a definition says one of them",
                ));
            }
            self.advance();
            self.expect_word("replace", "`REPLACE` after `OR`")?;
            if self.peek_keyword() == Some(Keyword::If) {
                return Err(self.error_here(
                    "the name — `OR REPLACE` replaces what is there and `IF NOT EXISTS` \
                     keeps it, so a definition says one of them",
                ));
            }
        }
        Ok((keep, replace))
    }

    fn eat_if_not_exists(&mut self) -> Result<bool> {
        if !self.eat_keyword(Keyword::If) {
            return Ok(false);
        }
        self.expect_keyword(Keyword::Not, "`NOT` after `IF`")?;
        self.expect_keyword(Keyword::Exists, "`EXISTS` after `IF NOT`")?;
        Ok(true)
    }

    /// The failure for whatever stands under the cursor.
    /// Run `rule` one expression deeper, refusing past the ceiling.
    ///
    /// The reader is recursive descent, and a statement nested without bound
    /// would end the process by exhausting the stack: an outage rather than a
    /// refusal, and one any client could cause with a long enough line.
    fn nested<T>(&mut self, rule: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_EXPRESSION_DEPTH {
            return Err(Error::NestedTooDeep {
                limit: MAX_EXPRESSION_DEPTH,
                span: self.span_here(),
            });
        }
        self.depth = self.depth.saturating_add(1);
        let answer = rule(self);
        self.depth = self.depth.saturating_sub(1);
        answer
    }

    fn error_here(&self, expected: &'static str) -> Error {
        self.error_at(self.position, expected)
    }

    /// The refusal for the token at `position`, which a rule that has already
    /// read past a word uses to point back at it.
    fn error_at(&self, position: usize, expected: &'static str) -> Error {
        let Some(spanned) = self.tokens.get(position) else {
            return Error::UnexpectedEnd {
                expected,
                span: self.end_of_source(),
            };
        };
        if let Token::Ident(word) = &spanned.token
            && let Some(feature) = absent_feature(word)
        {
            return Error::Unsupported {
                feature,
                span: spanned.span,
            };
        }
        Error::UnexpectedToken {
            found: describe(&spanned.token),
            expected,
            span: spanned.span,
        }
    }
}

/// The words that name something `docs/tessariql.md` §8 leaves out on purpose.
///
/// They are looked up here rather than reserved as keywords, so that a table
/// called `order` stays legal while `ORDER BY` still gets an answer that names
/// the reason instead of complaining about a semicolon.
///
/// # An entry leaves when the thing it names is built
///
/// This is the same rule §8's own table follows, and it has to be stated here
/// because this list is a **second copy** of that table and copies drift. It
/// drifted: an audit of §8 found `search` and `knn` still here after both were
/// built, so a caller who typed either was told a feature was missing while the
/// store had it. Nothing raised — the message is only read by somebody who
/// already made a mistake, so a wrong one goes unseen for as long as it exists.
///
/// `search` had additionally become **unreachable**: once `SEARCH` was reserved
/// the lexer stopped producing an identifier for it, so the entry could not have
/// fired even had it been true. That is worth noticing about any entry here — a
/// word that becomes a keyword leaves this list by the same rule.
///
/// Lifted out of the function body so the tests below can walk it. A list whose
/// entries nothing checks is how this one came to hold two false ones.
const ABSENT: &[(&str, &str)] = &[
    // `JOIN` is built. `INNER` and `LEFT` are not: a bare `JOIN` is inner, and
    // `LEFT` is the additive change that default was chosen to leave room for.
    ("inner", "a join qualifier — a bare `JOIN` is already inner"),
    ("left", "an outer join"),
    ("having", "filtering groups"),
    // The **spelling** is what is absent, not the feature. `START 5 LIMIT 2`
    // parses; an entry saying "limiting a result" told an author a built thing
    // was missing, which is the same failure `search` had and the reason this
    // table is re-audited rather than trusted (§8 names the spelling, not the
    // feature).
    ("offset", "a second spelling for `START`"),
    // `GRANT` and `REVOKE` are built, **including on fields**:
    // `GRANT read ON staff FIELDS name, title TO ada` parses. So the absent
    // thing is the clause, not the capability — §8 lists two narrower absences
    // (a field grant on a nested route, and one that limits writing) and neither
    // is what an author writing `PERMISSIONS` is reaching for.
    (
        "permissions",
        "a `PERMISSIONS` clause — field access is granted with `GRANT … FIELDS`",
    ),
];

/// The feature `word` names, when `docs/tessariql.md` §8 leaves it out on purpose.
fn absent_feature(word: &str) -> Option<&'static str> {
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
        Token::Parameter(name) => format!("the parameter `${name}`"),
        Token::Number(_) => "a number".to_owned(),
        Token::Str(_) => "text".to_owned(),
        Token::Bytes(_) => "bytes".to_owned(),
        Token::Duration(_) => "a duration".to_owned(),
        Token::Punct(punct) => format!("`{punct}`"),
    }
}

#[cfg(test)]
mod tests;
