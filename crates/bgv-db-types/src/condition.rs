//! What an operator means, once both sides are values.
//!
//! # Why this is here and not in the language crate
//!
//! Because two crates need it and neither is above the other. The session
//! evaluates `WHERE balance > 0`; the **store** evaluates
//! `ASSERT $value > 0` on its apply path, where validation has to live so that a
//! replica reaches the same verdict from the record alone. Two implementations
//! of `>` would eventually disagree, and the disagreement would be a write one
//! node refuses and another accepts — so there is one, and it sits below both.
//!
//! [`BinaryOp`] moved down with it for the same reason and one of its own: these
//! are not grammar, they are *the questions the value system answers about two
//! values*, and the total order that `<` and `>` **are** is already declared
//! here.
//!
//! # Comparison is the value system's order, not SQL's
//!
//! `docs/value-system.md` §3 declares a **total order across types**, and it
//! declares it for a reason that is not about querying at all: an index holding
//! a column with more than one type in it has to sort somehow, and "unspecified"
//! is not an answer a range scan can use. This module exposes that same order to
//! `<` and `>`, which has two consequences worth stating rather than
//! discovering.
//!
//! **There is no three-valued logic.** A comparison answers true or false and
//! never "unknown", so `NOT (x = 5)` holds for a record with no `x`. SQL would
//! say unknown; this store says the field is absent, absent is a value, and
//! absent is not five.
//!
//! **A comparison can cross types.** `age > 18` holds for a record whose `age`
//! is the text `'nineteen'`, because a string ranks above a number. Surprising
//! once and consistent forever — and the alternative is a comparison that
//! disagrees with the order its own index is stored in, which is the failure
//! this store keeps refusing: an answer that changes when an index appears.
//! `SCHEMAFULL` with `TYPE int` is how a table stops holding both.
//!
//! **Except that `none` and `null` are not small values.** The declared order
//! does place them below everything, and following it here would make
//! `age <= 17` find every record with no age recorded — a data mistake dressed
//! as an answer. So an *ordered* comparison against either of them is false,
//! while equality still sees them as themselves. That is the whole of why the
//! language needs no `IS NULL`: `age = NONE` and `age = NULL` are two different
//! questions and both are already sayable.

use crate::Value;

/// An operator taking two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `=` — the two values are the same value.
    Equal,
    /// `!=` — they are not.
    NotEqual,
    /// `<` — below, in the value system's declared order across types.
    Less,
    /// `<=` — below or the same.
    LessOrEqual,
    /// `>` — above.
    Greater,
    /// `>=` — above or the same.
    GreaterOrEqual,
    /// `IN` — the collection on the **right** holds the value on the left.
    ///
    /// The mirror of [`BinaryOp::Contains`], and both exist because both read
    /// naturally in different sentences: `'urgent' IN tags` and
    /// `tags CONTAINS 'urgent'` ask the same question from either end.
    In,
    /// `CONTAINS` — the collection on the **left** holds the value on the right.
    ///
    /// Membership, not substring — a different question from [`BinaryOp::Like`],
    /// which is why both exist. `tags CONTAINS 'urgent'` asks whether an array
    /// or a set holds that element; `body LIKE '%urgent%'` asks whether text
    /// contains those characters.
    Contains,
    /// `LIKE` — the text matches a pattern, as SQL's `LIKE` does.
    ///
    /// The pattern covers the **whole** value — which is why a substring search
    /// is written `'%text%'` — with `%` standing for any run of characters and
    /// `_` for exactly one.
    Like,
    /// The same, ignoring case.
    Ilike,
    /// `MATCHES` — the analyzed text holds every term of the query.
    ///
    /// A third question, not a special case of the other two: `LIKE` is a
    /// pattern over the whole value and `CONTAINS` is membership in a
    /// collection, and neither can ask whether text holds a *word*.
    Matches,
}

impl BinaryOp {
    /// How the operator is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::In => "IN",
            Self::Contains => "CONTAINS",
            Self::Like => "LIKE",
            Self::Ilike => "ILIKE",
            Self::Matches => "MATCHES",
        }
    }

    /// The operator a spelling names, if any.
    ///
    /// The inverse of [`BinaryOp::spelling`], and it exists because an
    /// [`Assertion`] is stored in the catalog as an ordinary record: the
    /// operator travels as the text it is written with rather than as a number
    /// somebody would have to keep a table for.
    #[must_use]
    pub fn from_spelling(spelling: &str) -> Option<Self> {
        [
            Self::Equal,
            Self::NotEqual,
            Self::Less,
            Self::LessOrEqual,
            Self::Greater,
            Self::GreaterOrEqual,
            Self::In,
            Self::Contains,
            Self::Like,
            Self::Ilike,
            Self::Matches,
        ]
        .into_iter()
        .find(|held| held.spelling() == spelling)
    }
}

/// Whether two values satisfy an operator.
///
/// Total in both arguments: every operator has an answer for every pair, which
/// is what lets a filter over documents of differing shapes run without either
/// raising or silently skipping.
pub fn apply(op: BinaryOp, left: &Value, right: &Value) -> bool {
    match op {
        BinaryOp::Equal => left == right,
        BinaryOp::NotEqual => left != right,
        // An ordered comparison against a non-value is false, on both sides.
        // The declared order does place `none` below `null` below everything
        // else, and exposing that here would make `age <= 17` find every record
        // with no age recorded — a data mistake dressed as an answer. Absent and
        // null are not small values; they are the absence of one, and "is the
        // absence of a value below seventeen" is not a question with an answer.
        // Equality still sees them as themselves, which is what makes
        // `age = NONE` and `age = NULL` the two things the language says instead
        // of `IS NULL`.
        BinaryOp::Less | BinaryOp::LessOrEqual | BinaryOp::Greater | BinaryOp::GreaterOrEqual
            if !left.is_present()
                || !right.is_present()
                || *left == Value::Null
                || *right == Value::Null =>
        {
            false
        }
        BinaryOp::Less => left < right,
        BinaryOp::LessOrEqual => left <= right,
        BinaryOp::Greater => left > right,
        BinaryOp::GreaterOrEqual => left >= right,
        // The same question from either end, which is why the language has both
        // spellings: `'urgent' IN tags` and `tags CONTAINS 'urgent'`.
        BinaryOp::In => holds(right, left),
        BinaryOp::Contains => holds(left, right),
        BinaryOp::Like => like(left, right, false),
        BinaryOp::Ilike => like(left, right, true),
        // A term match needs the field's analyzer, which is schema rather than
        // value, so the evaluator answers it before reaching here. Left as a
        // stated `false` rather than an `unreachable!()`: this project has none,
        // and "no analyzer, no terms, no match" is the same answer a field with
        // no analyzer gets anyway.
        BinaryOp::Matches => false,
    }
}

/// Membership: does this collection hold that value.
///
/// A different question from [`BinaryOp::Like`], which is why the language has
/// both. Only a collection answers it — a field holding a single value is not a
/// one-element collection, because treating it as one would make
/// `name CONTAINS 'ada'` quietly mean `name = 'ada'` and hide a mistake in the
/// query rather than showing it as no match.
fn holds(collection: &Value, wanted: &Value) -> bool {
    match collection {
        Value::Array(items) => items.contains(wanted),
        Value::Set(items) => items.contains(wanted),
        _ => false,
    }
}

/// SQL's `LIKE`, over the whole value.
///
/// Only text matches a text pattern: a number in that field is not an error, it
/// simply does not satisfy the test. Deliberately no tokenising, stemming or
/// ranking — those belong to an analyzer, and a scan-shaped approximation of one
/// would give answers a real text index later disagrees with.
fn like(held: &Value, wanted: &Value, fold_case: bool) -> bool {
    let (Value::String(text), Value::String(pattern)) = (held, wanted) else {
        return false;
    };
    if fold_case {
        matches_pattern(&text.to_lowercase(), &pattern.to_lowercase())
    } else {
        matches_pattern(text, pattern)
    }
}

/// `%` stands for any run of characters, `_` for exactly one, and `\\` escapes
/// either of them.
///
/// Iterative with a single backtrack point rather than recursive: a pattern of
/// many `%` would otherwise cost exponentially in the length of the text, which
/// is a denial of service written by whoever typed the query.
fn matches_pattern(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut t, mut p) = (0_usize, 0_usize);
    let (mut star_at, mut resume) = (None, 0_usize);

    while t < text.len() {
        let current = pattern.get(p).copied();
        let escaped = current == Some('\\');
        let literal = if escaped {
            pattern.get(p.saturating_add(1)).copied()
        } else {
            current
        };
        match (current, literal) {
            (Some('%'), _) if !escaped => {
                star_at = Some(p);
                p = p.saturating_add(1);
                resume = t;
            }
            (Some('_'), _) if !escaped => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            (Some(_), Some(want)) if text.get(t).copied() == Some(want) => {
                p = p.saturating_add(if escaped { 2 } else { 1 });
                t = t.saturating_add(1);
            }
            _ => {
                // No match here. Give the last `%` one more character and retry.
                let Some(star) = star_at else {
                    return false;
                };
                resume = resume.saturating_add(1);
                t = resume;
                p = star.saturating_add(1);
            }
        }
    }
    pattern
        .get(p..)
        .is_some_and(|rest| rest.iter().all(|c| *c == '%'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{BinaryOp, apply, matches_pattern};
    use crate::{Number, Value};

    fn text(value: &str) -> Value {
        Value::String(value.to_owned())
    }

    #[test]
    fn a_pattern_covers_the_whole_value() {
        assert!(matches_pattern("ada", "ada"));
        assert!(!matches_pattern("ada lovelace", "ada"));
        assert!(matches_pattern("ada lovelace", "ada%"));
        assert!(matches_pattern("ada lovelace", "%lovelace"));
        assert!(matches_pattern("ada lovelace", "%love%"));
        assert!(!matches_pattern("ada", ""));
        assert!(matches_pattern("", ""));
        assert!(matches_pattern("", "%"));
    }

    #[test]
    fn an_underscore_stands_for_exactly_one_character() {
        assert!(matches_pattern("ada", "ad_"));
        assert!(!matches_pattern("ad", "ad_"));
        assert!(!matches_pattern("adam", "ad_"));
    }

    #[test]
    fn a_backslash_makes_a_wildcard_literal() {
        assert!(matches_pattern("100%", "100\\%"));
        assert!(!matches_pattern("100x", "100\\%"));
        assert!(matches_pattern("a_b", "a\\_b"));
        assert!(!matches_pattern("axb", "a\\_b"));
    }

    #[test]
    fn many_wildcards_do_not_cost_exponentially() {
        // The reason the matcher backtracks from one remembered star rather than
        // recursing: this pattern against this text is the classic blow-up, and
        // it is written by whoever typed the query.
        let text = "a".repeat(64);
        assert!(!matches_pattern(&text, "%a%a%a%a%a%a%a%a%b"));
        assert!(matches_pattern(&text, "%a%a%a%a%a%a%a%a%a"));
    }

    #[test]
    fn trailing_wildcards_match_nothing_at_all() {
        assert!(matches_pattern("ada", "ada%"));
        assert!(matches_pattern("ada", "ada%%%"));
        assert!(!matches_pattern("ada", "ada_"));
    }

    #[test]
    fn comparison_follows_the_declared_order_across_types() {
        // Not a quirk of the implementation: `docs/value-system.md` §3 declares
        // it, an index range read depends on it, and a comparison that
        // disagreed with it would answer differently once an index existed.
        let number = Value::Number(Number::Integer(18));
        assert!(apply(BinaryOp::Greater, &text("nineteen"), &number));
        // …and an ordered comparison against a non-value is false rather than
        // placing absence below everything, which would make `age <= 17` find
        // every record with no age recorded.
        assert!(!apply(BinaryOp::Less, &Value::None, &number));
        assert!(!apply(BinaryOp::LessOrEqual, &Value::Null, &number));
        assert!(!apply(BinaryOp::Greater, &number, &Value::None));
    }

    #[test]
    fn absent_and_null_compare_as_the_distinct_values_they_are() {
        assert!(apply(BinaryOp::Equal, &Value::None, &Value::None));
        assert!(apply(BinaryOp::NotEqual, &Value::None, &Value::Null));
    }

    #[test]
    fn membership_reads_from_either_end_and_means_one_thing() {
        let tags = Value::Array(vec![text("urgent"), text("old")]);
        assert!(apply(BinaryOp::Contains, &tags, &text("urgent")));
        assert!(apply(BinaryOp::In, &text("urgent"), &tags));
        // A single value is not a one-element collection.
        assert!(!apply(BinaryOp::Contains, &text("urgent"), &text("urgent")));
    }
}
