//! How many writers a namespace admits.
//!
//! Separate from [`Replication`], which answers how many *copies* the cluster
//! keeps, because the two are independent facts about one namespace: a namespace
//! declared multi-master still has a factor, and a namespace with a factor of
//! three still has to say whether three copies means three readers or three
//! writers. Folding the class into the count would make one of them unsayable.
//!
//! # Why absence is not one of the variants
//!
//! The catalog holds an `Option<ReplicationClass>`, and the `None` of that
//! option means **never stated**. It reads as single-leader everywhere, which is
//! the honest reading of every namespace that existed before the clause did and
//! the same reading an absent epoch flag has — not a fallback, because
//! single-leader is what the engine has always done and what the refusal this
//! class scopes has always enforced. Nothing on disk is rewritten.
//!
//! [`ReplicationClass::SingleLeader`] is therefore not redundant with silence:
//! it is a namespace whose operator considered the question and answered it,
//! which `INFO FOR` can report as a decision rather than as a default.
//!
//! [`Replication`]: crate::Replication

use core::fmt;

use crate::Value;

/// The word a single-leader namespace is stored and written as.
const SINGLE_LEADER: &str = "single-leader";

/// The word a multi-master namespace is stored and written as.
const MULTI_MASTER: &str = "multi-master";

/// How many writers a namespace admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReplicationClass {
    /// One leadership writes this namespace; every other node replicates it.
    SingleLeader,
    /// Two or more leaderships write it, and a conflict between them is named
    /// rather than ranked.
    MultiMaster,
}

impl ReplicationClass {
    /// Whether this class admits more than one writer.
    ///
    /// The one question every caller actually asks, so it is asked once here
    /// rather than matched at each site — a match repeated at four call sites is
    /// four places to forget a variant added later.
    #[must_use]
    pub const fn admits_two_writers(self) -> bool {
        matches!(self, Self::MultiMaster)
    }

    /// The value written to the catalog, and read back by `INFO`.
    ///
    /// One encoding for both, for [`crate::Replication::to_value`]'s reason:
    /// what the catalog stores is what a reader is shown, so the stored fact and
    /// the reported fact cannot disagree.
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::SingleLeader => Value::from(SINGLE_LEADER),
            Self::MultiMaster => Value::from(MULTI_MASTER),
        }
    }

    /// Read one back.
    ///
    /// Answers `None` for anything else rather than falling back to
    /// single-leader. A namespace stored by a later build under a class this one
    /// does not know must not be read as though it used the class this build
    /// happens to prefer — and here the cost of getting that wrong is admitting
    /// a second writer to a range that never declared one, or refusing one that
    /// did.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(word) if word == SINGLE_LEADER => Some(Self::SingleLeader),
            Value::String(word) if word == MULTI_MASTER => Some(Self::MultiMaster),
            _ => None,
        }
    }
}

impl fmt::Display for ReplicationClass {
    /// The clause as a statement writes it, so a message naming a class names
    /// something the reader can type back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleLeader => f.write_str("SINGLE LEADER"),
            Self::MultiMaster => f.write_str("MULTI MASTER"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReplicationClass;
    use crate::Value;

    #[test]
    fn every_class_survives_the_value_it_is_stored_as() {
        for class in [
            ReplicationClass::SingleLeader,
            ReplicationClass::MultiMaster,
        ] {
            assert_eq!(ReplicationClass::from_value(&class.to_value()), Some(class));
        }
    }

    #[test]
    fn an_unrecognised_class_is_refused_rather_than_read_as_single_leader() {
        // The failure this guards is silent in both directions: a class a later
        // build wrote, read as single-leader here, refuses a writer the operator
        // declared; read as multi-master, it admits one they never did.
        for word in ["singleleader", "SINGLE-LEADER", "primary", ""] {
            assert_eq!(
                ReplicationClass::from_value(&Value::from(word)),
                None,
                "{word} is not a class this build knows"
            );
        }
        assert_eq!(ReplicationClass::from_value(&Value::Null), None);
        assert_eq!(ReplicationClass::from_value(&Value::from(1_i64)), None);
    }

    #[test]
    fn only_multi_master_admits_two_writers() {
        assert!(ReplicationClass::MultiMaster.admits_two_writers());
        assert!(!ReplicationClass::SingleLeader.admits_two_writers());
    }

    #[test]
    fn a_class_is_displayed_as_the_clause_that_declares_it() {
        assert_eq!(ReplicationClass::MultiMaster.to_string(), "MULTI MASTER");
        assert_eq!(ReplicationClass::SingleLeader.to_string(), "SINGLE LEADER");
    }
}
