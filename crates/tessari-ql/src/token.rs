//! What the language is made of, before it means anything.
//!
//! A token carries its **span** — where it started and ended in the source — and
//! that is not decoration. A parse error whose message cannot point at the
//! offending characters makes the caller re-read their own script guessing, and
//! a query language is written by hand more often than any other interface this
//! store has.

mod keyword;
use core::fmt;

pub use keyword::Keyword;
use tessari_types::{Duration, Number};

/// Where a token sits in the source, as byte offsets.
///
/// Byte offsets rather than character positions, because that is what indexes
/// the source text; a caller rendering a caret converts once, at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    /// The first byte of the token.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
}

impl Span {
    /// A span over `start..end`.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// How many bytes the token occupies.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span covers nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The span reaching from this one's start to `end`'s end.
    ///
    /// What a parser needs to give a node built from several tokens a span
    /// covering all of them.
    #[must_use]
    pub const fn to(self, end: Self) -> Self {
        Self::new(self.start, end.end)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// A token together with where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    /// The token itself.
    pub token: Token,
    /// Where it sits in the source.
    pub span: Span,
}

impl Spanned {
    /// Pair a token with its span.
    #[must_use]
    pub const fn new(token: Token, span: Span) -> Self {
        Self { token, span }
    }
}

/// One lexical unit.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Token {
    /// A reserved word, matched without regard to case.
    Keyword(Keyword),
    /// A name: a namespace, table, field, or index.
    Ident(String),
    /// A parameter the caller binds a value to, written `$name`.
    ///
    /// Lexically distinct from a name because it is grammatically distinct: a
    /// parameter stands where a **literal** stands and nowhere a name does, and
    /// that rule is what makes a bound value unable to become syntax.
    Parameter(String),
    /// An integer or a float. The exact-decimal marker is a keyword, so a
    /// decimal arrives here as the number the keyword applies to.
    Number(Number),
    /// Text, with its escapes already resolved.
    Str(String),
    /// Opaque bytes, written `0x…`.
    Bytes(Vec<u8>),
    /// A span of time, written as digits and a unit.
    Duration(Duration),
    /// Punctuation and operators.
    Punct(Punct),
}

/// Punctuation and operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Punct {
    /// `;` — ends a statement.
    Semicolon,
    /// `,` — separates items.
    Comma,
    /// `:` — joins a table and a record id.
    Colon,
    /// `.` — qualifies a name.
    Dot,
    /// `..` — an exclusive range.
    DotDot,
    /// `..=` — an inclusive range.
    DotDotEquals,
    /// `=` — assignment, and equality in a condition.
    Equals,
    /// `!=` — inequality.
    NotEquals,
    /// `<` — below, in the value system's declared order.
    Less,
    /// `<=` — below or equal.
    LessOrEqual,
    /// `>` — above.
    Greater,
    /// `>=` — above or equal.
    GreaterOrEqual,
    /// `*` — every field, and multiplication.
    Star,
    /// `+` — addition.
    Plus,
    /// `-` — subtraction, and negation.
    Minus,
    /// `/` — division.
    Slash,
    /// `%` — remainder.
    Percent,
    /// `??` — the left value unless it holds nothing.
    Coalesce,
    /// `::` — separates a function's group from its name.
    ColonColon,
    /// `{`
    BraceOpen,
    /// `}`
    BraceClose,
    /// `[`
    BracketOpen,
    /// `]`
    BracketClose,
    /// `->` — a traversal step following an edge from its source.
    ArrowRight,
    /// `<-` — a traversal step following an edge from its target.
    ArrowLeft,
    /// `|` — separates the members of a declared union of literals.
    ///
    /// Not a boolean or, which is the word `OR`, and not a bitwise one, which
    /// this language does not have. It appears in exactly one position — after
    /// `TYPE` — so the single character costs the language nothing elsewhere.
    Pipe,
    /// `(`
    ParenOpen,
    /// `)`
    ParenClose,
}

impl Punct {
    /// How this punctuation is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Semicolon => ";",
            Self::Comma => ",",
            Self::Colon => ":",
            Self::Dot => ".",
            Self::DotDot => "..",
            Self::DotDotEquals => "..=",
            Self::Equals => "=",
            Self::NotEquals => "!=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Star => "*",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::Coalesce => "??",
            Self::ColonColon => "::",
            Self::BraceOpen => "{",
            Self::BraceClose => "}",
            Self::BracketOpen => "[",
            Self::BracketClose => "]",
            Self::ArrowRight => "->",
            Self::ArrowLeft => "<-",
            Self::Pipe => "|",
            Self::ParenOpen => "(",
            Self::ParenClose => ")",
        }
    }
}

impl fmt::Display for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.spelling())
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.spelling())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_keyword_is_recognised_in_any_case_and_is_unique() {
        for keyword in Keyword::ALL {
            let spelling = keyword.spelling();
            assert_eq!(Keyword::from_word(spelling), Some(*keyword));
            assert_eq!(Keyword::from_word(&spelling.to_lowercase()), Some(*keyword));
        }
        let mut spellings: Vec<&str> = Keyword::ALL.iter().map(|k| k.spelling()).collect();
        spellings.sort_unstable();
        let count = spellings.len();
        spellings.dedup();
        assert_eq!(spellings.len(), count, "two keywords share a spelling");
    }

    #[test]
    fn a_word_that_is_not_reserved_is_not_a_keyword() {
        assert_eq!(Keyword::from_word("users"), None);
        assert_eq!(Keyword::from_word(""), None);
    }
}
