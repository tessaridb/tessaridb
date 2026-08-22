//! What a field declaration demands of the value, beyond its type.
//!
//! # Why this is a closed vocabulary and not an expression
//!
//! `ASSERT` is checked on the **store's apply path**, beside the type check and
//! for the same reasons: the verdict must be a pure function of the record and
//! the catalog, so a replica reaches it without anything being sent, and a
//! refusal must fail the whole commit.
//!
//! An arbitrary expression is not a pure function of the record — `time::now()`
//! is not, and a read is not — so a store holding one would have to *check* that
//! the expression it was given happens to be pure, every time, for ever. A closed
//! vocabulary is pure, total and cheap **by construction**, and what cannot be
//! lowered into it is refused where it is written rather than where it runs.
//!
//! This is not a second expression language. There is one source spelling —
//! bgvQL — and one lowering, which is exactly the move `TYPE decimal` already
//! makes into a [`crate::FieldKind`]. What the store holds is a compiled form,
//! the way an index entry is a compiled form of a value.
//!
//! # The one binding
//!
//! `$value` is the value being checked. It is the only parameter an assertion
//! may name, and the reason is the `DEFAULT` rule read forwards: a declaration
//! belongs to no call, so nothing can bind a parameter in one. `$value` is the
//! exception because the **store** binds it, once per record, on the apply path.

use std::collections::BTreeMap;

use crate::condition::{BinaryOp, apply};
use crate::value::Value;

/// An object from its named fields.
fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(BTreeMap::from(
        fields.map(|(name, held)| (name.to_owned(), held)),
    ))
}

const ASSERT_OP: &str = "op";
const ASSERT_AGAINST: &str = "against";
const ASSERT_ALL: &str = "all";
const ASSERT_ANY: &str = "any";
const ASSERT_NOT: &str = "not";

/// A constraint on one field's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assertion {
    /// `$value <op> <literal>`.
    Compare {
        /// The operator, meaning exactly what it means in a `WHERE`.
        op: BinaryOp,
        /// The literal on the right.
        against: Value,
    },
    /// Every part holds. `$value > 0 AND $value < 150`.
    All(Vec<Assertion>),
    /// Some part holds.
    Any(Vec<Assertion>),
    /// The part does not hold.
    Not(Box<Assertion>),
}

impl Assertion {
    /// Whether a value satisfies this constraint.
    ///
    /// The comparison is [`apply`], which is the same function a `WHERE` uses —
    /// so `ASSERT $value > 0` and `WHERE balance > 0` cannot disagree about a
    /// value, and a write refused by one node cannot be accepted by another.
    #[must_use]
    pub fn holds(&self, value: &Value) -> bool {
        match self {
            Self::Compare { op, against } => apply(*op, value, against),
            Self::All(parts) => parts.iter().all(|part| part.holds(value)),
            Self::Any(parts) => parts.iter().any(|part| part.holds(value)),
            Self::Not(part) => !part.holds(value),
        }
    }

    /// The constraint as an ordinary value, for the catalog record it rides in.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Compare { op, against } => object([
                (ASSERT_OP, Value::String(op.spelling().to_owned())),
                (ASSERT_AGAINST, against.clone()),
            ]),
            Self::All(parts) => object([(ASSERT_ALL, Self::list(parts))]),
            Self::Any(parts) => object([(ASSERT_ANY, Self::list(parts))]),
            Self::Not(part) => object([(ASSERT_NOT, part.to_value())]),
        }
    }

    /// The constraint a stored value describes, or `None` when it describes
    /// none.
    ///
    /// Total rather than raising: a catalog record that does not describe an
    /// assertion is a corrupt one, and the caller decides what that means.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let Value::Object(fields) = value else {
            return None;
        };
        if let Some(parts) = fields.get(ASSERT_ALL) {
            return Some(Self::All(Self::parts(parts)?));
        }
        if let Some(parts) = fields.get(ASSERT_ANY) {
            return Some(Self::Any(Self::parts(parts)?));
        }
        if let Some(part) = fields.get(ASSERT_NOT) {
            return Some(Self::Not(Box::new(Self::from_value(part)?)));
        }
        let Some(Value::String(spelling)) = fields.get(ASSERT_OP) else {
            return None;
        };
        Some(Self::Compare {
            op: BinaryOp::from_spelling(spelling)?,
            against: fields.get(ASSERT_AGAINST)?.clone(),
        })
    }

    fn list(parts: &[Self]) -> Value {
        Value::Array(parts.iter().map(Self::to_value).collect())
    }

    fn parts(value: &Value) -> Option<Vec<Self>> {
        let Value::Array(items) = value else {
            return None;
        };
        items.iter().map(Self::from_value).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Assertion;
    use crate::condition::BinaryOp;
    use crate::number::Number;
    use crate::value::Value;

    fn number(held: i64) -> Value {
        Value::Number(Number::Integer(held))
    }

    fn positive() -> Assertion {
        Assertion::Compare {
            op: BinaryOp::Greater,
            against: number(0),
        }
    }

    #[test]
    fn a_comparison_is_the_one_a_filter_would_make() {
        assert!(positive().holds(&number(1)));
        assert!(!positive().holds(&number(0)));
        // An ordered comparison against a non-value is false — the same rule a
        // `WHERE` follows, because it is the same function.
        assert!(!positive().holds(&Value::None));
    }

    #[test]
    fn a_range_is_two_comparisons() {
        let range = Assertion::All(vec![
            positive(),
            Assertion::Compare {
                op: BinaryOp::Less,
                against: number(150),
            },
        ]);
        assert!(range.holds(&number(30)));
        assert!(!range.holds(&number(200)));
        assert!(!range.holds(&number(0)));
    }

    #[test]
    fn a_stored_assertion_comes_back_as_itself() {
        // It rides in a catalog record, so it has to survive the round trip that
        // every other thing in one does.
        for held in [
            positive(),
            Assertion::All(vec![positive(), positive()]),
            Assertion::Any(vec![positive()]),
            Assertion::Not(Box::new(positive())),
            Assertion::Compare {
                op: BinaryOp::In,
                against: Value::Array(vec![number(1), number(2)]),
            },
        ] {
            assert_eq!(Assertion::from_value(&held.to_value()), Some(held));
        }
    }

    #[test]
    fn a_value_that_describes_no_assertion_is_not_one() {
        assert_eq!(Assertion::from_value(&number(3)), None);
        assert_eq!(
            Assertion::from_value(&Value::Object(Default::default())),
            None
        );
    }
}
