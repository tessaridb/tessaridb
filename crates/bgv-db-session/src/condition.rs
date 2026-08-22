//! The two questions about a condition that are the session's own.
//!
//! What an operator *means* once both sides are values is
//! `bgv_db_types::condition`, and it lives there rather than here because the
//! **store** needs it too: `ASSERT` is checked on the apply path, where
//! validation has to live so a replica reaches the same verdict from the record
//! alone. Two implementations of `>` would eventually disagree, and the
//! disagreement would be a write one node refuses and another accepts.
//!
//! What is left here is what only a session asks:
//!
//! **A condition is a boolean.** Every operator answers with one, so a condition
//! can only be non-boolean when the author wrote a bare path or literal in that
//! position. `WHERE tags` is not a question with a false answer; it is a
//! question that was not finished, and an error naming what was found says so
//! where an empty result would hide it.
//!
//! **A pattern sometimes has a prefix.** Which is a planner question — what an
//! index can be asked — rather than a question about what `LIKE` means.

use bgv_db_ql::Span;
use bgv_db_types::Value;

use crate::error::{Error, Result};

/// The truth a condition states, or a failure naming what stood there instead.
pub(crate) fn boolean(value: &Value, span: Span) -> Result<bool> {
    match value {
        Value::Bool(held) => Ok(*held),
        other => Err(Error::ConditionNotBoolean {
            found: other.type_name(),
            span,
        }),
    }
}

/// The literal a pattern begins with, when the pattern is exactly that literal
/// followed by a trailing `%`.
///
/// Only that shape. For it, "begins with the literal" and "matches the pattern"
/// are the same statement, so a range read over the index needs no second test
/// and cannot answer differently from a scan. `'%a%'`, `'a_b%'` and `'a%b'` are
/// all left to the scan rather than served from a bound that would be a guess.
///
/// The escape is honoured, so `'50\%%'` asks for values beginning with `50%`.
pub(crate) fn literal_prefix(pattern: &str) -> Option<String> {
    let mut literal = String::new();
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => literal.push(characters.next()?),
            '_' => return None,
            '%' => {
                return match (characters.next(), literal.is_empty()) {
                    // A trailing `%` and something before it.
                    (None, false) => Some(literal),
                    _ => None,
                };
            }
            other => literal.push(other),
        }
    }
    // No wildcard at all. That is an equality written the long way, and it is
    // left alone rather than quietly rewritten into one.
    None
}
