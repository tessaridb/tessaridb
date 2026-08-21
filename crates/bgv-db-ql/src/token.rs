//! What the language is made of, before it means anything.
//!
//! A token carries its **span** — where it started and ended in the source — and
//! that is not decoration. A parse error whose message cannot point at the
//! offending characters makes the caller re-read their own script guessing, and
//! a query language is written by hand more often than any other interface this
//! store has.

use core::fmt;

use bgv_db_types::{Duration, Number};

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
    /// `=` — assignment, and equality in a filter.
    Equals,
    /// `*` — every field.
    Star,
    /// `{`
    BraceOpen,
    /// `}`
    BraceClose,
    /// `[`
    BracketOpen,
    /// `]`
    BracketClose,
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
            Self::Star => "*",
            Self::BraceOpen => "{",
            Self::BraceClose => "}",
            Self::BracketOpen => "[",
            Self::BracketClose => "]",
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

/// A reserved word.
///
/// Reserved words are matched case-insensitively and are **not** available as
/// names. The alternative — contextual keywords — buys convenience and pays for
/// it with a grammar whose meaning depends on where you are standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Keyword {
    /// `USE`
    Use,
    /// `NAMESPACE`
    Namespace,
    /// `DATABASE`
    Database,
    /// `DEFINE`
    Define,
    /// `DROP`
    Drop,
    /// `TABLE`
    Table,
    /// `SPACE`
    Space,
    /// `INDEX`
    Index,
    /// `ON`
    On,
    /// `FIELDS`
    Fields,
    /// `UNIQUE`
    Unique,
    /// `IF`
    If,
    /// `NOT`
    Not,
    /// `EXISTS`
    Exists,
    /// `CREATE`
    Create,
    /// `SELECT`
    Select,
    /// `FROM`
    From,
    /// `WHERE`
    Where,
    /// `UPDATE`
    Update,
    /// `DELETE`
    Delete,
    /// `GET`
    Get,
    /// `SET` — both the key-value verb and the set-literal marker; which one is
    /// decided by whether a `[` follows.
    Set,
    /// `DEL`
    Del,
    /// `KEYS`
    Keys,
    /// `RANGE`
    Range,
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `CANCEL`
    Cancel,
    /// `NONE` — the field is not there.
    None,
    /// `NULL` — the field is there and holds nothing.
    Null,
    /// `TRUE`
    True,
    /// `FALSE`
    False,
    /// `DEC` — marks the number after it as an exact decimal.
    Dec,
    /// `DATETIME` — marks the string after it as a point in time.
    Datetime,
    /// `UUID` — marks the string after it as sixteen bytes.
    Uuid,
}

impl Keyword {
    /// The reserved word this keyword is written as, in upper case.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Use => "USE",
            Self::Namespace => "NAMESPACE",
            Self::Database => "DATABASE",
            Self::Define => "DEFINE",
            Self::Drop => "DROP",
            Self::Table => "TABLE",
            Self::Space => "SPACE",
            Self::Index => "INDEX",
            Self::On => "ON",
            Self::Fields => "FIELDS",
            Self::Unique => "UNIQUE",
            Self::If => "IF",
            Self::Not => "NOT",
            Self::Exists => "EXISTS",
            Self::Create => "CREATE",
            Self::Select => "SELECT",
            Self::From => "FROM",
            Self::Where => "WHERE",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Get => "GET",
            Self::Set => "SET",
            Self::Del => "DEL",
            Self::Keys => "KEYS",
            Self::Range => "RANGE",
            Self::Begin => "BEGIN",
            Self::Commit => "COMMIT",
            Self::Cancel => "CANCEL",
            Self::None => "NONE",
            Self::Null => "NULL",
            Self::True => "TRUE",
            Self::False => "FALSE",
            Self::Dec => "DEC",
            Self::Datetime => "DATETIME",
            Self::Uuid => "UUID",
        }
    }

    /// Every reserved word, so that the lookup and this list cannot drift.
    pub const ALL: &'static [Self] = &[
        Self::Use,
        Self::Namespace,
        Self::Database,
        Self::Define,
        Self::Drop,
        Self::Table,
        Self::Space,
        Self::Index,
        Self::On,
        Self::Fields,
        Self::Unique,
        Self::If,
        Self::Not,
        Self::Exists,
        Self::Create,
        Self::Select,
        Self::From,
        Self::Where,
        Self::Update,
        Self::Delete,
        Self::Get,
        Self::Set,
        Self::Del,
        Self::Keys,
        Self::Range,
        Self::Begin,
        Self::Commit,
        Self::Cancel,
        Self::None,
        Self::Null,
        Self::True,
        Self::False,
        Self::Dec,
        Self::Datetime,
        Self::Uuid,
    ];

    /// The keyword a word spells, ignoring case.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|keyword| keyword.spelling().eq_ignore_ascii_case(word))
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
