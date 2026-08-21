//! Failures raised while reading a script.
//!
//! Every variant carries a [`Span`], because a message that cannot point at the
//! offending characters leaves the author of a hand-written query re-reading it
//! and guessing.

use crate::token::Span;

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure reading a script.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A character that starts nothing the language knows.
    #[error("unexpected character {found:?} at {span}")]
    UnexpectedCharacter {
        /// The character found.
        found: char,
        /// Where it is.
        span: Span,
    },

    /// A string literal reached the end of the script without closing.
    #[error("unterminated string starting at {span}")]
    UnterminatedString {
        /// Where the string began.
        span: Span,
    },

    /// A backslash escape this language does not define.
    ///
    /// Refused rather than passed through: a script that writes `\d` meaning a
    /// literal backslash and a `d` would otherwise silently lose the backslash,
    /// and the value stored would differ from the value written.
    #[error("invalid escape \\{found} at {span}")]
    InvalidEscape {
        /// The character after the backslash.
        found: char,
        /// Where the escape is.
        span: Span,
    },

    /// A number the language cannot represent.
    #[error("{text:?} at {span} is not a number this store can hold")]
    InvalidNumber {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A byte literal that is not whole bytes of hexadecimal.
    #[error("{reason} in byte literal at {span}")]
    InvalidBytes {
        /// What was wrong: an odd digit count, or a non-hexadecimal digit.
        reason: &'static str,
        /// Where it is.
        span: Span,
    },

    /// A duration with an unknown unit, or one too large to represent.
    #[error("{text:?} at {span} is not a duration this store can hold")]
    InvalidDuration {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A token the grammar does not allow where it stands.
    #[error("expected {expected} at {span}, found {found}")]
    UnexpectedToken {
        /// What was there, as the language spells it.
        found: String,
        /// What would have been accepted.
        expected: &'static str,
        /// Where it is.
        span: Span,
    },

    /// The script ended in the middle of a statement.
    #[error("expected {expected}, but the script ended at {span}")]
    UnexpectedEnd {
        /// What would have been accepted.
        expected: &'static str,
        /// The end of the source.
        span: Span,
    },

    /// Something the language will have and this milestone does not.
    ///
    /// Separate from [`Error::UnexpectedToken`] because the two say different
    /// things to whoever reads them: one is a typo, the other is a feature that
    /// is absent on purpose, and telling an author to check their spelling when
    /// the answer is "not yet" wastes their afternoon.
    #[error("{feature} is not in this milestone (at {span})")]
    Unsupported {
        /// The absent feature, named as `docs/bgvql.md` §8 names it.
        feature: &'static str,
        /// Where it was asked for.
        span: Span,
    },

    /// Text after `datetime` that is not an instant.
    #[error("{text:?} at {span} is not an instant: expected RFC 3339, as in 1970-01-01T00:00:00Z")]
    InvalidDatetime {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// Text after `uuid` that is not sixteen bytes.
    #[error("{text:?} at {span} is not a uuid")]
    InvalidUuid {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A number after `dec` that no exact decimal can hold.
    #[error("{text:?} at {span} is not a decimal this store can hold exactly")]
    InvalidDecimal {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A record identity that is not one of the four kinds a record id has.
    #[error("a record id is an integer, text, a uuid or bytes (at {span})")]
    InvalidRecordId {
        /// Where the identity was written.
        span: Span,
    },

    /// `RANGE` followed by something that is not a range.
    #[error("expected a range such as 'a'..'m' at {span}")]
    NotARange {
        /// Where the expression was written.
        span: Span,
    },

    /// An object literal that names one field twice.
    ///
    /// Refused rather than resolved: keeping either occurrence stores a value
    /// the author did not write, and nothing downstream can tell which one was
    /// meant.
    #[error("field {name:?} is written twice in one object (at {span})")]
    DuplicateField {
        /// The field's name.
        name: String,
        /// Where the second occurrence is.
        span: Span,
    },
}

impl Error {
    /// Where the failure is, for a caller rendering a caret.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::UnexpectedCharacter { span, .. }
            | Self::UnterminatedString { span }
            | Self::InvalidEscape { span, .. }
            | Self::InvalidNumber { span, .. }
            | Self::InvalidBytes { span, .. }
            | Self::InvalidDuration { span, .. }
            | Self::UnexpectedToken { span, .. }
            | Self::UnexpectedEnd { span, .. }
            | Self::Unsupported { span, .. }
            | Self::InvalidDatetime { span, .. }
            | Self::InvalidUuid { span, .. }
            | Self::InvalidDecimal { span, .. }
            | Self::InvalidRecordId { span }
            | Self::NotARange { span }
            | Self::DuplicateField { span, .. } => *span,
        }
    }
}
