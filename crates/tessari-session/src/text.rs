//! Taking a string apart, and putting one back together.
//!
//! # Characters, never bytes
//!
//! `string::len` already counts characters rather than bytes, for the reason its
//! comment gives: a caller asking how long a name is means the name and not its
//! encoding. The two functions here that take a position follow it, and they
//! have to — a position counted in bytes can land in the middle of a character,
//! and the answer is then either a panic or a broken string. Neither is a thing
//! a query language should be able to produce.
//!
//! # An empty separator is refused rather than interpreted
//!
//! `string::split(text, '')` and `string::replace(text, '', x)` each have a
//! defensible reading and no obvious one: the first could mean "into
//! characters", the second could mean "insert between every character". Both are
//! different functions from the ones asked for, so the empty separator is a
//! caller mistake and is named as one.

use tessari_ql::{Function, Span};
use tessari_types::Value;

use crate::error::{Error, Result};

/// The parts of `text` between occurrences of `separator`.
///
/// A separator that does not occur gives one part, the whole text — which is the
/// answer that composes: `array::first(string::split(x, '@'))` is the part
/// before the separator whether or not there is one.
pub(crate) fn split(text: &str, separator: &str, span: Span) -> Result<Value> {
    if separator.is_empty() {
        return Err(Error::CallFailed {
            function: Function::StringSplit,
            reason: "a separator is at least one character",
            span,
        });
    }
    Ok(Value::Array(
        text.split(separator).map(Value::from).collect(),
    ))
}

/// A run of `count` characters starting at `start`.
///
/// The same bounds rule `array::slice` follows, and deliberately the same: a
/// start past the end is an empty string and a count reaching past it takes what
/// is there, while a negative bound is refused. Two functions spelled `slice`
/// that disagreed about what a position means would be worse than one of them
/// not existing.
pub(crate) fn slice(text: &str, start: i64, count: i64, span: Span) -> Result<Value> {
    let refused = |reason: &'static str| Error::CallFailed {
        function: Function::StringSlice,
        reason,
        span,
    };
    if start < 0 {
        return Err(refused("a slice starts at or after the first character"));
    }
    if count < 0 {
        return Err(refused("a slice holds no fewer than no characters"));
    }
    let held: String = usize::try_from(start).map_or_else(
        |_| String::new(),
        |from| {
            text.chars()
                .skip(from)
                .take(usize::try_from(count).unwrap_or(usize::MAX))
                .collect()
        },
    );
    Ok(Value::from(held.as_str()))
}

/// Every occurrence of `from` replaced by `to`.
///
/// **Every** occurrence, not the first. A function replacing one would need to
/// say which, and "the first" is a choice the caller did not make; replacing all
/// is the answer that does not depend on where in the text they happened to be.
pub(crate) fn replace(text: &str, from: &str, to: &str, span: Span) -> Result<Value> {
    if from.is_empty() {
        return Err(Error::CallFailed {
            function: Function::StringReplace,
            reason: "the text to replace is at least one character",
            span,
        });
    }
    Ok(Value::from(text.replace(from, to).as_str()))
}

#[cfg(test)]
mod tests {
    use tessari_ql::Span;
    use tessari_types::Value;

    use super::{replace, slice, split};

    fn at() -> Span {
        Span::new(0, 1)
    }

    #[test]
    fn splitting_on_a_separator_that_is_absent_gives_the_whole_text() {
        // The answer that composes: taking the first part is the part before
        // the separator whether or not there is one.
        assert_eq!(
            split("a@b", "@", at()).expect("parts"),
            Value::Array(vec![Value::from("a"), Value::from("b")])
        );
        assert_eq!(
            split("ada", "@", at()).expect("parts"),
            Value::Array(vec![Value::from("ada")])
        );
    }

    #[test]
    fn an_empty_separator_is_refused_rather_than_read_as_into_characters() {
        assert!(split("abc", "", at()).is_err());
        assert!(replace("abc", "", "-", at()).is_err());
    }

    #[test]
    fn a_position_counts_characters_and_not_bytes() {
        // The failure this prevents: a byte position lands inside a character
        // and the answer is a panic or a broken string. `héllo` is six bytes
        // and five characters.
        assert_eq!(
            slice("héllo", 1, 2, at()).expect("a slice"),
            Value::from("él")
        );
        assert_eq!(
            slice("héllo", 0, 5, at()).expect("a slice"),
            Value::from("héllo")
        );
    }

    #[test]
    fn a_text_slice_follows_the_same_bounds_rule_an_array_slice_does() {
        assert_eq!(
            slice("abc", 2, 10, at()).expect("a slice"),
            Value::from("c")
        );
        assert_eq!(slice("abc", 9, 1, at()).expect("a slice"), Value::from(""));
        assert!(slice("abc", -1, 1, at()).is_err());
        assert!(slice("abc", 0, -1, at()).is_err());
    }

    #[test]
    fn replacing_takes_every_occurrence_and_not_the_first() {
        assert_eq!(
            replace("a-b-c", "-", "+", at()).expect("text"),
            Value::from("a+b+c")
        );
        // A `to` that contains the `from` does not loop: the scan moves past
        // what it wrote.
        assert_eq!(
            replace("aa", "a", "aa", at()).expect("text"),
            Value::from("aaaa")
        );
    }
}
