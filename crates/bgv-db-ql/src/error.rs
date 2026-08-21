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
            | Self::InvalidDuration { span, .. } => *span,
        }
    }
}
