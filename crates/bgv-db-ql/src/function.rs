//! The functions the language has, and how many arguments each one takes.
//!
//! # What earns a place
//!
//! **A function is here when it cannot be expressed by what the language already
//! has.** That is why there is no `array::contains` (`CONTAINS` says it), no
//! `string::contains` (`LIKE '%x%'` says it), and no `is_none` (`= NONE` says
//! it). The rule keeps the surface from growing by association and gives a
//! reviewer one question to ask about any addition.
//!
//! `array::last` is the clearest case for the rule rather than against it: a
//! path takes a literal position, and there is no length to subtract from, so
//! "the last element" is genuinely unsayable without it.
//!
//! # Why a name is namespaced
//!
//! `string::len` rather than `len`, for three reasons: the function surface can
//! never collide with a field name, the set is groupable in the documentation,
//! and an aggregate called `count` later does not have to argue with
//! `array::len`. It costs one punctuation token.
//!
//! # Why arity lives here and types do not
//!
//! The set of functions is known when a statement is read, so a call with the
//! wrong number of arguments is a mistake that can be refused before anything
//! runs. What each argument *holds* is not known until a record is in hand, so
//! that check belongs where the value is.

use core::fmt;

/// One of the language's own functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Function {
    /// `string::len(text)` — how many characters, not bytes.
    StringLen,
    /// `string::lower(text)`
    StringLower,
    /// `string::upper(text)`
    StringUpper,
    /// `string::trim(text)` — whitespace from both ends.
    StringTrim,
    /// `string::concat(a, b)`
    StringConcat,
    /// `array::len(items)`
    ArrayLen,
    /// `array::first(items)` — `none` when there are none.
    ArrayFirst,
    /// `array::last(items)` — the one a path cannot reach.
    ArrayLast,
    /// `math::abs(number)`
    MathAbs,
    /// `math::floor(number)`
    MathFloor,
    /// `math::ceil(number)`
    MathCeil,
    /// `math::round(number)` — half away from zero.
    MathRound,
    /// `time::now()` — the instant the statement is evaluated at.
    TimeNow,
    /// `type::of(value)` — the type's name, as §3 spells it.
    TypeOf,
}

impl Function {
    /// Every function, so that a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::StringLen,
        Self::StringLower,
        Self::StringUpper,
        Self::StringTrim,
        Self::StringConcat,
        Self::ArrayLen,
        Self::ArrayFirst,
        Self::ArrayLast,
        Self::MathAbs,
        Self::MathFloor,
        Self::MathCeil,
        Self::MathRound,
        Self::TimeNow,
        Self::TypeOf,
    ];

    /// How the function is written, group and name together.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::StringLen => "string::len",
            Self::StringLower => "string::lower",
            Self::StringUpper => "string::upper",
            Self::StringTrim => "string::trim",
            Self::StringConcat => "string::concat",
            Self::ArrayLen => "array::len",
            Self::ArrayFirst => "array::first",
            Self::ArrayLast => "array::last",
            Self::MathAbs => "math::abs",
            Self::MathFloor => "math::floor",
            Self::MathCeil => "math::ceil",
            Self::MathRound => "math::round",
            Self::TimeNow => "time::now",
            Self::TypeOf => "type::of",
        }
    }

    /// How many arguments it takes.
    #[must_use]
    pub const fn arity(self) -> usize {
        match self {
            Self::TimeNow => 0,
            Self::StringConcat => 2,
            _ => 1,
        }
    }

    /// The function a `group::name` spells, if there is one.
    #[must_use]
    pub fn parse(spelling: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|function| function.spelling() == spelling)
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.spelling())
    }
}

#[cfg(test)]
mod tests {
    use super::Function;

    #[test]
    fn every_function_is_findable_by_its_own_spelling_and_no_two_share_one() {
        let mut spellings: Vec<&str> = Function::ALL
            .iter()
            .map(|function| {
                assert_eq!(Function::parse(function.spelling()), Some(*function));
                function.spelling()
            })
            .collect();
        let count = spellings.len();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), count, "two functions share a spelling");
    }

    #[test]
    fn a_name_that_is_not_a_function_is_not_one() {
        // Case-sensitive, unlike a keyword: a function name is a name, and every
        // other name in this language is case-sensitive too.
        assert_eq!(Function::parse("string::length"), None);
        assert_eq!(Function::parse("STRING::LEN"), None);
        assert_eq!(Function::parse("len"), None);
    }
}
