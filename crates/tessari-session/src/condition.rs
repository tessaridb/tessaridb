//! The two questions about a condition that are the session's own.
//!
//! What an operator *means* once both sides are values is
//! `tessari_types::condition`, and it lives there rather than here because the
//! **store** needs it too: `ASSERT` is checked on the apply path, where
//! validation has to live so a replica reaches the same verdict from the record
//! alone. Two implementations of `>` would eventually disagree, and the
//! disagreement would be a write one node refuses and another accepts.
//!
//! What is left here is what only a session asks:
//!
//! **A condition is a boolean, or it is an absence.** Every operator answers
//! with a boolean, so a condition can only be some *other* type when the author
//! wrote a bare path or literal in that position. `WHERE tags` is not a question
//! with a false answer; it is a question that was not finished, and an error
//! naming what was found says so where an empty result would hide it.
//!
//! An **absence is different, and it is not an error**. A function of an absence
//! is an absence (`tessari_session::call`), which is what lets a read over
//! records of differing shapes narrow instead of failing — and the only place
//! that rule is ever exercised is a condition. Refusing `none` here would make
//! the two rules contradict each other exactly where they meet: one record
//! missing one field would fail the whole read, which is the outcome the absence
//! rule exists to prevent.
//!
//! So an absent or null condition is **false**: the record did not answer the
//! question, and a record that did not answer it is not one of the records that
//! did. That is the same resolution SQL reaches through three-valued logic, and
//! it is deliberately narrow — every type that is neither a boolean nor an
//! absence is still refused.
//!
//! **A pattern sometimes has a prefix.** Which is a planner question — what an
//! index can be asked — rather than a question about what `LIKE` means.

use tessari_ql::Span;
use tessari_types::Value;

use crate::error::{Error, Result};

/// The truth a condition states, or a failure naming what stood there instead.
pub(crate) fn boolean(value: &Value, span: Span) -> Result<bool> {
    match value {
        Value::Bool(held) => Ok(*held),
        // The record did not answer the question, so it is not one of the
        // records that answered it yes.
        Value::None | Value::Null => Ok(false),
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
