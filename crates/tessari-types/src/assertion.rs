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
//! TessariQL — and one lowering, which is exactly the move `TYPE decimal` already
//! makes into a [`crate::FieldKind`]. What the store holds is a compiled form,
//! the way an index entry is a compiled form of a value.
//!
//! # The one binding
//!
//! `$value` is the value being checked. It is the only parameter an assertion
//! may name, and the reason is the `DEFAULT` rule read forwards: a declaration
//! belongs to no call, so nothing can bind a parameter in one. `$value` is the
//! exception because the **store** binds it, once per record, on the apply path.
//!
//! # The other side of the comparison
//!
//! The right-hand side is an [`Operand`]: a literal, or a **route into the same
//! record**. The second form is what lets a declaration say *`ends_at` must be
//! after `starts_at`*, and it widens the constraint's *subject* from one value to
//! one record without making it an expression — the route is data, resolved by
//! [`Path::resolve`], and there is still nothing to evaluate.
//!
//! It is not a second parameter, which is why the paragraph above still holds: a
//! bare route is what a `WHERE` already means by the same spelling, so the two
//! read the record the same way and cannot disagree about it.
//!
//! It costs nothing at the write. The store already holds the whole decoded
//! record when it checks a field, so resolving a route into it is a borrow of a
//! value that is already in hand — no read, no allocation, and no difference in
//! cost between a satisfied assertion and a violated one.

use std::collections::BTreeMap;

use crate::condition::{BinaryOp, apply};
use crate::path::Path;
use crate::value::Value;

/// An object from its named fields.
fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(BTreeMap::from(
        fields.map(|(name, held)| (name.to_owned(), held)),
    ))
}

const ASSERT_OP: &str = "op";
const ASSERT_AGAINST: &str = "against";
const ASSERT_FIELD: &str = "field";
const ASSERT_ALL: &str = "all";
const ASSERT_ANY: &str = "any";
const ASSERT_NOT: &str = "not";

/// What a comparison is against.
///
/// A separate key on the wire for each form rather than one key holding either,
/// because `against` may hold an object and a route encoded as an object would
/// be indistinguishable from one. The separation also makes the older reader do
/// the right thing for free: a binary that predates routes finds no `against`,
/// reads no assertion, and the catalog refuses the definition as malformed
/// rather than silently enforcing nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// A value written in the statement.
    Literal(Value),
    /// A route to another value in the same record.
    ///
    /// Never a route holding `[*]`: that reaches several values and a comparison
    /// wants one, so it is refused where it is written.
    Field(Path),
}

/// A constraint on one field's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assertion {
    /// `$value <op> <literal>`, or `$value <op> <field of the same record>`.
    Compare {
        /// The operator, meaning exactly what it means in a `WHERE`.
        op: BinaryOp,
        /// What is on the right.
        against: Operand,
    },
    /// Every part holds. `$value > 0 AND $value < 150`.
    All(Vec<Assertion>),
    /// Some part holds.
    Any(Vec<Assertion>),
    /// The part does not hold.
    Not(Box<Assertion>),
}

impl Assertion {
    /// Whether a value satisfies this constraint, within the record it sits in.
    ///
    /// The comparison is [`apply`], which is the same function a `WHERE` uses —
    /// so `ASSERT $value > 0` and `WHERE balance > 0` cannot disagree about a
    /// value, and a write refused by one node cannot be accepted by another.
    ///
    /// A route that reaches nothing is handed to [`apply`] as `NONE`, and what
    /// happens next is [`apply`]'s existing rule rather than a new one: an
    /// ordered comparison against an absence is **false**, so
    /// `ASSERT $value > starts_at` on a record with no `starts_at` refuses the
    /// write. That is the same answer `WHERE` gives for the same question, and
    /// having one answer is the point.
    #[must_use]
    pub fn holds(&self, value: &Value, record: &Value) -> bool {
        match self {
            Self::Compare { op, against } => {
                let right = match against {
                    Operand::Literal(held) => held,
                    Operand::Field(route) => route.resolve(record).unwrap_or(&Value::None),
                };
                apply(*op, value, right)
            }
            Self::All(parts) => parts.iter().all(|part| part.holds(value, record)),
            Self::Any(parts) => parts.iter().any(|part| part.holds(value, record)),
            Self::Not(part) => !part.holds(value, record),
        }
    }

    /// The other fields of the record this constraint compares against, in the
    /// order it names them.
    ///
    /// For a refusal to name, so that a message says which two fields disagreed
    /// rather than only which one was written. Empty for a constraint that
    /// compares against literals alone, which is every constraint that existed
    /// before routes did.
    #[must_use]
    pub fn compared_fields(&self) -> Vec<String> {
        let mut named = Vec::new();
        self.gather_compared(&mut named);
        named
    }

    fn gather_compared(&self, into: &mut Vec<String>) {
        match self {
            Self::Compare {
                against: Operand::Field(route),
                ..
            } => into.push(route.to_string()),
            Self::Compare { .. } => {}
            Self::All(parts) | Self::Any(parts) => {
                for part in parts {
                    part.gather_compared(into);
                }
            }
            Self::Not(part) => part.gather_compared(into),
        }
    }

    /// The constraint as an ordinary value, for the catalog record it rides in.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Compare { op, against } => {
                let spelling = Value::String(op.spelling().to_owned());
                match against {
                    // Unchanged from before routes existed, deliberately: a
                    // literal assertion already on disk re-encodes to the same
                    // bytes, so nothing has to be migrated.
                    Operand::Literal(held) => {
                        object([(ASSERT_OP, spelling), (ASSERT_AGAINST, held.clone())])
                    }
                    Operand::Field(route) => object([
                        (ASSERT_OP, spelling),
                        (ASSERT_FIELD, Value::String(route.to_string())),
                    ]),
                }
            }
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
        let against = match fields.get(ASSERT_FIELD) {
            Some(Value::String(written)) => {
                let route = Path::parse(written)?;
                // A route reaching several values has no single value to compare,
                // so one here describes no assertion. Refused at both doors —
                // this one and the lowering — rather than resolving to nothing
                // and quietly becoming a constraint that always refuses.
                if route.is_several() {
                    return None;
                }
                Operand::Field(route)
            }
            Some(_) => return None,
            None => Operand::Literal(fields.get(ASSERT_AGAINST)?.clone()),
        };
        Some(Self::Compare {
            op: BinaryOp::from_spelling(spelling)?,
            against,
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
    #![allow(clippy::panic)]

    use super::{ASSERT_AGAINST, ASSERT_FIELD, ASSERT_OP, Assertion, Operand, object};
    use crate::condition::BinaryOp;
    use crate::number::Number;
    use crate::path::Path;
    use crate::value::Value;

    fn number(held: i64) -> Value {
        Value::Number(Number::Integer(held))
    }

    fn literal(held: Value) -> Operand {
        Operand::Literal(held)
    }

    fn route(spelling: &str) -> Operand {
        Operand::Field(Path::parse(spelling).expect("a route"))
    }

    fn positive() -> Assertion {
        Assertion::Compare {
            op: BinaryOp::Greater,
            against: literal(number(0)),
        }
    }

    /// A record for the assertions that do not read one.
    fn anything() -> Value {
        Value::Object(Default::default())
    }

    fn booking(starts_at: Value) -> Value {
        object([("starts_at", starts_at)])
    }

    #[test]
    fn a_comparison_is_the_one_a_filter_would_make() {
        assert!(positive().holds(&number(1), &anything()));
        assert!(!positive().holds(&number(0), &anything()));
        // An ordered comparison against a non-value is false — the same rule a
        // `WHERE` follows, because it is the same function.
        assert!(!positive().holds(&Value::None, &anything()));
    }

    #[test]
    fn a_range_is_two_comparisons() {
        let range = Assertion::All(vec![
            positive(),
            Assertion::Compare {
                op: BinaryOp::Less,
                against: literal(number(150)),
            },
        ]);
        assert!(range.holds(&number(30), &anything()));
        assert!(!range.holds(&number(200), &anything()));
        assert!(!range.holds(&number(0), &anything()));
    }

    fn after_it_starts() -> Assertion {
        Assertion::Compare {
            op: BinaryOp::Greater,
            against: route("starts_at"),
        }
    }

    #[test]
    fn a_comparison_may_name_another_field_of_the_same_record() {
        // The check the language could not say before: one field against another.
        assert!(after_it_starts().holds(&number(10), &booking(number(5))));
        assert!(!after_it_starts().holds(&number(5), &booking(number(10))));
    }

    #[test]
    fn a_route_that_reaches_nothing_refuses_rather_than_passing() {
        // Not a rule of this module's own: an ordered comparison against an
        // absence is false in `apply`, and both spellings of "no value there"
        // arrive at it the same way. A declaration that cannot be satisfied
        // refusing the write is the safe direction of that answer.
        assert!(!after_it_starts().holds(&number(10), &Value::Object(Default::default())));
        assert!(!after_it_starts().holds(&number(10), &booking(Value::None)));
        assert!(!after_it_starts().holds(&number(10), &booking(Value::Null)));
    }

    #[test]
    fn a_route_may_reach_into_a_nested_value() {
        let record = object([("window", object([("from", number(5))]))]);
        let after_the_window = Assertion::Compare {
            op: BinaryOp::Greater,
            against: route("window.from"),
        };
        assert!(after_the_window.holds(&number(10), &record));
        assert!(!after_the_window.holds(&number(1), &record));
    }

    #[test]
    fn a_route_composes_with_the_other_forms() {
        let both = Assertion::All(vec![positive(), after_it_starts()]);
        assert!(both.holds(&number(10), &booking(number(5))));
        assert!(!both.holds(&number(-1), &booking(number(-5))));

        assert!(Assertion::Not(Box::new(after_it_starts())).holds(&number(1), &booking(number(5))));
        assert!(
            Assertion::Any(vec![after_it_starts(), positive()])
                .holds(&number(1), &booking(number(5)))
        );
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
                against: literal(Value::Array(vec![number(1), number(2)])),
            },
            after_it_starts(),
            Assertion::All(vec![after_it_starts(), positive()]),
            Assertion::Compare {
                op: BinaryOp::Equal,
                against: route("window.from"),
            },
        ] {
            assert_eq!(Assertion::from_value(&held.to_value()), Some(held));
        }
    }

    #[test]
    fn a_literal_assertion_still_encodes_exactly_as_it_did() {
        // Nothing on disk is migrated by this, and that claim is only worth
        // making if it is checked against the bytes rather than the round trip:
        // a codec can be self-consistently wrong.
        assert_eq!(
            positive().to_value(),
            object([
                (ASSERT_OP, Value::String(">".to_owned())),
                (ASSERT_AGAINST, number(0)),
            ])
        );
    }

    #[test]
    fn a_route_is_written_under_its_own_key_so_an_older_reader_refuses_it() {
        // The forward-compatibility claim, stated as the property that produces
        // it. A binary that predates routes decodes a comparison by reading
        // `against`; this form has none, so that binary reads no assertion and
        // the catalog raises `CatalogMalformed` rather than enforcing nothing.
        let encoded = after_it_starts().to_value();
        let Value::Object(fields) = &encoded else {
            panic!("an assertion encodes as an object");
        };
        assert!(!fields.contains_key(ASSERT_AGAINST));
        assert_eq!(
            fields.get(ASSERT_FIELD),
            Some(&Value::String("starts_at".to_owned()))
        );
    }

    #[test]
    fn a_value_that_describes_no_assertion_is_not_one() {
        assert_eq!(Assertion::from_value(&number(3)), None);
        assert_eq!(
            Assertion::from_value(&Value::Object(Default::default())),
            None
        );
    }

    #[test]
    fn a_route_reaching_several_values_describes_no_assertion() {
        // A comparison wants one value. `tags[*]` reaches many, so it is refused
        // here as well as at the lowering — the two doors a stored assertion can
        // arrive through.
        let several = object([
            (ASSERT_OP, Value::String(">".to_owned())),
            (ASSERT_FIELD, Value::String("tags[*]".to_owned())),
        ]);
        assert_eq!(Assertion::from_value(&several), None);
    }

    #[test]
    fn a_route_that_is_not_text_describes_no_assertion() {
        let wrong = object([
            (ASSERT_OP, Value::String(">".to_owned())),
            (ASSERT_FIELD, number(3)),
        ]);
        assert_eq!(Assertion::from_value(&wrong), None);
    }
}
