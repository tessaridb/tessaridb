//! What a table does with a write it cannot order.
//!
//! Separate from [`ReplicationClass`], which answers how many writers a
//! *namespace* admits, because the two are facts about different things and sit
//! at different levels on purpose. A namespace says whether a second writer may
//! exist at all; a table says what to do when two of them have written the same
//! record without seeing each other. A counter can tolerate a dropped update
//! beside a ledger row in the same namespace that cannot, and a single setting
//! for both would make one of them wrong.
//!
//! # Why absence is not one of the variants
//!
//! The catalog holds an `Option<ConflictPolicy>`, and the `None` of that option
//! means **never stated**. It reads as [`ConflictPolicy::Refuse`] everywhere,
//! which is the honest reading of every table that existed before the clause did
//! — not a fallback, because refusing is what this engine does under ADR-0075
//! and what a table with no declaration has always had done for it. Nothing on
//! disk is rewritten.
//!
//! [`ConflictPolicy::Refuse`] is therefore not redundant with silence: it is a
//! table whose operator considered the question and answered it, which `INFO
//! FOR` can report as a decision rather than as a default.
//!
//! # Last-writer-wins here reads no clock, and that is deliberate
//!
//! In the field, last-writer-wins means comparing timestamps and keeping the
//! newest. That is the one shape this engine must not take: a causal stamp
//! answers *did this write see that one*, a clock answers *which happened
//! later*, and a store doing both has two answers to one question with no rule
//! about which is authoritative. The reference implementation of this pattern
//! publishes a product advisory saying exactly that about enabling its version
//! vectors and its last-write-wins setting together.
//!
//! So the rule here is not a comparison. A concurrency is met on the **commit**
//! path, at the moment somebody is writing — and the last writer is that caller.
//! The incoming write supersedes every surviving version including the ones it
//! never saw, and the survivors it does not descend are the writes that were
//! discarded. No clock is read, nothing has to agree across nodes, and the
//! reconciling write reaches every replica after both contested versions, so it
//! is newest everywhere.
//!
//! [`ReplicationClass`]: crate::ReplicationClass

use core::fmt;

use crate::Value;

/// The word a refusing table is stored and written as.
const REFUSE: &str = "refuse";

/// The word a last-writer-wins table is stored and written as.
const LAST_WRITER_WINS: &str = "last-writer-wins";

/// What a table does with a write it cannot order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConflictPolicy {
    /// Name the concurrency and refuse the write; the caller reconciles.
    Refuse,
    /// Take the write, discard what it did not see, and count what was
    /// discarded.
    LastWriterWins,
}

impl ConflictPolicy {
    /// Whether this policy takes the write instead of refusing it.
    ///
    /// The one question every caller actually asks, so it is asked once here
    /// rather than matched at each site — for the reason
    /// [`crate::ReplicationClass::admits_two_writers`] gives.
    #[must_use]
    pub const fn discards_the_loser(self) -> bool {
        matches!(self, Self::LastWriterWins)
    }

    /// The value written to the catalog, and read back by `INFO`.
    ///
    /// One encoding for both, so the stored fact and the reported fact cannot
    /// disagree.
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::Refuse => Value::from(REFUSE),
            Self::LastWriterWins => Value::from(LAST_WRITER_WINS),
        }
    }

    /// Read one back.
    ///
    /// Answers `None` for anything else rather than falling back to refusal. A
    /// table stored by a later build under a policy this one does not know must
    /// not be read as though it used the policy this build happens to prefer,
    /// and the cost of getting it wrong here is a write silently dropped on a
    /// table that never asked for that.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(word) if word == REFUSE => Some(Self::Refuse),
            Value::String(word) if word == LAST_WRITER_WINS => Some(Self::LastWriterWins),
            _ => None,
        }
    }
}

impl fmt::Display for ConflictPolicy {
    /// The clause as a statement writes it, so a message naming a policy names
    /// something the reader can type back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refuse => f.write_str("REFUSE CONFLICTS"),
            Self::LastWriterWins => f.write_str("LAST WRITER WINS"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConflictPolicy;
    use crate::Value;

    #[test]
    fn every_policy_survives_the_value_it_is_stored_as() {
        for policy in [ConflictPolicy::Refuse, ConflictPolicy::LastWriterWins] {
            assert_eq!(ConflictPolicy::from_value(&policy.to_value()), Some(policy));
        }
    }

    #[test]
    fn an_unrecognised_policy_is_refused_rather_than_read_as_refusal() {
        // Silent in both directions: a policy a later build wrote, read as
        // refusal here, refuses writes the operator asked to be taken; read as
        // last-writer-wins, it drops writes nobody agreed to lose.
        for word in ["lastwriterwins", "LAST-WRITER-WINS", "lww", ""] {
            assert_eq!(
                ConflictPolicy::from_value(&Value::from(word)),
                None,
                "{word} is not a policy this build knows"
            );
        }
        assert_eq!(ConflictPolicy::from_value(&Value::Null), None);
        assert_eq!(ConflictPolicy::from_value(&Value::from(1_i64)), None);
    }

    #[test]
    fn only_last_writer_wins_discards_the_loser() {
        assert!(ConflictPolicy::LastWriterWins.discards_the_loser());
        assert!(!ConflictPolicy::Refuse.discards_the_loser());
    }

    #[test]
    fn a_policy_is_displayed_as_the_clause_that_declares_it() {
        assert_eq!(
            ConflictPolicy::LastWriterWins.to_string(),
            "LAST WRITER WINS"
        );
        assert_eq!(ConflictPolicy::Refuse.to_string(), "REFUSE CONFLICTS");
    }
}
