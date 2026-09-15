//! How many copies of a namespace's data the cluster is asked to keep.
//!
//! The clause is declared where the namespace is declared (ADR-0060), because
//! the alternative is a cluster-wide default that every namespace inherits
//! without anybody having considered it — and a default nobody chose is
//! indistinguishable, afterwards, from a choice somebody made.
//!
//! # Why absence is not one of the variants
//!
//! The catalog holds an `Option<Replication>`, and the `None` of that option
//! means **never stated** while [`Replication::None`] means **stated, and the
//! answer is one copy**. They are different facts and they are not recoverable
//! from one another later: a namespace created before the clause existed said
//! nothing, and a namespace created with `REPLICATION NONE` declined. When the
//! cluster ships, the first is refused at the moment a second node would hold
//! it, and the second is honoured. Collapsing them into a single value is the
//! inherited default this vocabulary exists to abolish, wearing a costume.
//!
//! # What it deliberately cannot say yet
//!
//! Where the copies go. ADR-0060 argues that a clause naming only a count is
//! the migration that retrofitting placement into a counting clause becomes,
//! and it is right — but there are no nodes to name, so a placement vocabulary
//! written today would be invented rather than derived. What is bought is the
//! one property that cannot be added later: the difference between a namespace
//! that declined replication and one that was never asked.

use core::fmt;
use core::num::NonZeroU32;

use crate::Value;

/// The word a namespace declining replication is stored and written as.
const NONE: &str = "none";

/// What a namespace's `REPLICATION` clause says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Replication {
    /// `REPLICATION NONE` — this namespace is not to be replicated.
    None,
    /// `REPLICATION FACTOR 3` — how many copies of it the cluster keeps.
    ///
    /// `FACTOR 1` is accepted and describes the same number of copies as
    /// [`Self::None`]. The two are kept apart rather than folded together
    /// because they say different things about intent — one is a count that
    /// happens to be one, the other is a refusal — and because a script
    /// computing a factor should not have to special-case the value 1.
    ///
    /// Zero is refused where the clause is read: no copies at all is not a
    /// replication policy, it is the absence of the data.
    Factor(NonZeroU32),
}

impl Replication {
    /// The value written to the catalog, and read back by `INFO`.
    ///
    /// One encoding for both, so what the catalog stores is what a reader is
    /// shown: a word for the refusal and a plain number for the count. A
    /// two-encoding arrangement would let the stored fact and the reported fact
    /// disagree, which is the failure a definition that carries its own id
    /// already exists to avoid.
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::None => Value::from(NONE),
            Self::Factor(factor) => Value::from(i64::from(factor.get())),
        }
    }

    /// Read one back.
    ///
    /// Answers `None` for anything else — an unrecognised word, a number that
    /// is not a whole positive count — rather than falling back to a default,
    /// for the reason [`crate::IdentityKind::parse`] gives: a namespace stored
    /// by a later build under a policy this one does not know must not be read
    /// as though it used the policy this build happens to prefer.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(word) if word == NONE => Some(Self::None),
            Value::Number(number) => {
                NonZeroU32::new(u32::try_from(number.as_exact_integer()?).ok()?).map(Self::Factor)
            }
            _ => Option::None,
        }
    }
}

impl fmt::Display for Replication {
    /// The clause as a statement writes it, so a message naming a policy names
    /// something the reader can type back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("NONE"),
            Self::Factor(factor) => write!(f, "FACTOR {factor}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NonZeroU32, Replication};
    use crate::Value;

    fn factor(n: u32) -> Replication {
        Replication::Factor(NonZeroU32::new(n).expect("a non-zero factor"))
    }

    #[test]
    fn every_policy_round_trips_through_the_value_the_catalog_holds() {
        for policy in [Replication::None, factor(1), factor(3), factor(64)] {
            assert_eq!(
                Replication::from_value(&policy.to_value()),
                Some(policy),
                "{policy}"
            );
        }
    }

    #[test]
    fn a_policy_this_build_does_not_know_is_not_quietly_a_default() {
        // The failure this refuses is a namespace written by a later build
        // under a placement policy being read here as a bare count, which would
        // put its copies somewhere nobody asked for.
        assert_eq!(Replication::from_value(&Value::from("every-rack")), None);
        assert_eq!(Replication::from_value(&Value::from("NONE")), None);
        assert_eq!(Replication::from_value(&Value::Null), None);
        assert_eq!(Replication::from_value(&Value::None), None);
    }

    #[test]
    fn no_copies_at_all_is_not_a_policy() {
        // Zero would decode as a factor and mean the data is kept nowhere.
        assert_eq!(Replication::from_value(&Value::from(0_i64)), None);
    }

    #[test]
    fn declining_and_counting_to_one_stay_apart() {
        // The same number of copies, and deliberately not the same value: one
        // is a refusal to replicate, the other is a count.
        assert_ne!(Replication::None, factor(1));
        assert_ne!(Replication::None.to_value(), factor(1).to_value());
    }

    #[test]
    fn a_policy_prints_as_the_clause_that_declares_it() {
        assert_eq!(Replication::None.to_string(), "NONE");
        assert_eq!(factor(3).to_string(), "FACTOR 3");
    }
}
