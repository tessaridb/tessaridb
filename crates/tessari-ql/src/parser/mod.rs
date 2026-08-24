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
mod path;
mod shape;
mod statement;

use crate::ast::{Expr, Script};
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
        reading_paths: false,
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
    };
    let expression = parser.expression()?;
    if parser.peek().is_some() {
        return Err(parser.error_here("the end of the expression"));
    }
    Ok(expression)
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

    /// Whether the token `offset` places past the cursor is this one.
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
mod tests {
    use super::{ABSENT, absent_feature};
    use crate::token::Keyword;

    /// An entry whose word the lexer reserves can never fire.
    ///
    /// `search` sat here for several milestones after `SEARCH` became a keyword:
    /// the lexer stopped producing an identifier for it, so the lookup could not
    /// be reached, and the entry went on saying a built feature was missing. The
    /// two halves of that failure are separable — this one catches the
    /// unreachability, which is the half a reader cannot see.
    #[test]
    fn no_entry_names_a_word_the_lexer_reserves() {
        for (word, feature) in ABSENT {
            let reserved = Keyword::ALL
                .iter()
                .any(|keyword| keyword.spelling().eq_ignore_ascii_case(word));
            assert!(
                !reserved,
                "`{word}` ({feature}) is a keyword, so this entry can never fire"
            );
        }
    }

    /// The other half: a word naming something the store has.
    ///
    /// Pins the two the §8 audit found rather than the class, because the class
    /// has no test — nothing here can ask whether a feature exists. What keeps
    /// the rest honest is the audit, and what this asserts is that these two do
    /// not come back.
    #[test]
    fn a_feature_the_store_has_is_not_reported_absent() {
        assert_eq!(
            absent_feature("search"),
            None,
            "a search index is built — `DEFINE INDEX … SEARCH`"
        );
        assert_eq!(
            absent_feature("knn"),
            None,
            "vector search is built — `DEFINE INDEX … VECTOR euclidean` and `APPROXIMATE`"
        );
        // The two the §8 re-audit found (wave 38), pinned the same way and for
        // the same reason: both told an author that a **built** feature was
        // missing. `SELECT * FROM t OFFSET 5` answered "limiting a result is
        // not in this milestone" while `START 5 LIMIT 2` parses; `PERMISSIONS`
        // answered "per-field permissions" while
        // `GRANT read ON staff FIELDS name TO ada` parses.
        assert_ne!(
            absent_feature("offset"),
            Some("limiting a result"),
            "limiting a result is built — `START` and `LIMIT`"
        );
        assert_ne!(
            absent_feature("permissions"),
            Some("per-field permissions"),
            "per-field permissions are built — `GRANT … ON … FIELDS … TO …`"
        );
    }

    /// The list still does its job for what really is absent.
    ///
    /// The `OFFSET` line used to read `Some("limiting a result")`, and that is
    /// worth leaving a note about: **the test was holding the wrong message in
    /// place.** A word naming a built feature survived here precisely because
    /// something asserted it, and an assertion is as good at preserving a false
    /// statement as at preventing one. What each line pins now is the *spelling*
    /// being absent, which is what §8 actually says.
    #[test]
    fn a_word_naming_an_absent_feature_still_names_it() {
        assert_eq!(absent_feature("having"), Some("filtering groups"));
        assert_eq!(
            absent_feature("OFFSET"),
            Some("a second spelling for `START`")
        );
        assert_eq!(absent_feature("nothing_like_this"), None);
    }
}
