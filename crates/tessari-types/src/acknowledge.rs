//! How many copies must hold a write before its caller is told it happened
//! (ADR-0106 D1, D2).
//!
//! Named for what the caller is TOLD, because that is the whole contract: a
//! `LEADER` write is durable on the leader when it is acknowledged, a
//! `MAJORITY` write on a majority of the range's voters. Either way the write is
//! applied and readable on the leader at its local commit (D3); what the level
//! decides is when the answer comes back, and so what a failover can lose.

use core::fmt;

use crate::Value;

/// The word a leader-level acknowledgement is stored and written as.
const LEADER: &str = "leader";

/// The word a majority-level acknowledgement is stored and written as.
const MAJORITY: &str = "majority";

/// The suffix a namespace that admits weaker requests is stored with.
const OR_WEAKER: &str = " or weaker";

/// How many copies must hold a write before it is acknowledged.
///
/// Ordered weakest first, so "is this request weaker than the namespace's
/// level" is a comparison rather than a match a later level could slip past.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Acknowledge {
    /// Durable on the leader. A failover can lose it — the window the
    /// replication lag is wide.
    Leader,
    /// Durable on a majority of the range's voters, the leader counting as one.
    /// Any majority that elects the next leader meets it, so a failover keeps it.
    Majority,
}

impl Acknowledge {
    /// The value written to the catalog, and read back by `INFO`.
    #[must_use]
    pub fn to_value(self) -> Value {
        Value::from(self.word())
    }

    /// Read one back — `None` for a word this build does not know, rather than
    /// a level it happens to prefer: reading an unknown level as `LEADER` would
    /// acknowledge writes a failover can lose on a namespace that asked for more.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(word) => Self::from_word(word),
            _ => None,
        }
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Leader => LEADER,
            Self::Majority => MAJORITY,
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        match word {
            LEADER => Some(Self::Leader),
            MAJORITY => Some(Self::Majority),
            _ => None,
        }
    }
}

impl fmt::Display for Acknowledge {
    /// The clause as a statement writes it, so a message names something the
    /// reader can type back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Leader => f.write_str("ACKNOWLEDGE LEADER"),
            Self::Majority => f.write_str("ACKNOWLEDGE MAJORITY"),
        }
    }
}

/// A namespace's acknowledgement: its level, and whether a request may ask for
/// less (ADR-0106 D2).
///
/// `or_weaker` is the operator's statement that a caller may lower the level
/// for itself. Without it a weaker request is refused by name, because a
/// default any caller can silently lower is not a default anyone can rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Acknowledgement {
    /// The level a write in the namespace waits for unless it says otherwise.
    pub level: Acknowledge,
    /// Whether a request may ask for a weaker level than `level`.
    pub or_weaker: bool,
}

impl Acknowledgement {
    /// Whether a request asking for `asked` is admitted.
    #[must_use]
    pub fn admits(self, asked: Acknowledge) -> bool {
        asked >= self.level || self.or_weaker
    }

    /// The value written to the catalog, and read back by `INFO`.
    #[must_use]
    pub fn to_value(self) -> Value {
        let mut word = self.level.word().to_owned();
        if self.or_weaker {
            word.push_str(OR_WEAKER);
        }
        Value::from(word)
    }

    /// Read one back — `None` for anything this build did not write.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let Value::String(word) = value else {
            return None;
        };
        let (level, or_weaker) = word
            .strip_suffix(OR_WEAKER)
            .map_or((word.as_str(), false), |level| (level, true));
        Acknowledge::from_word(level).map(|level| Self { level, or_weaker })
    }
}

impl fmt::Display for Acknowledgement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.level)?;
        if self.or_weaker {
            f.write_str(" OR WEAKER")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Acknowledge, Acknowledgement};
    use crate::Value;

    #[test]
    fn every_acknowledgement_survives_the_value_it_is_stored_as() {
        for level in [Acknowledge::Leader, Acknowledge::Majority] {
            assert_eq!(Acknowledge::from_value(&level.to_value()), Some(level));
            for or_weaker in [false, true] {
                let stated = Acknowledgement { level, or_weaker };
                assert_eq!(
                    Acknowledgement::from_value(&stated.to_value()),
                    Some(stated)
                );
            }
        }
    }

    #[test]
    fn an_unknown_level_is_refused_rather_than_read_as_the_leaders() {
        for word in ["quorum", "MAJORITY", "all", "", "majority or stronger"] {
            assert_eq!(Acknowledge::from_value(&Value::from(word)), None, "{word}");
            assert_eq!(
                Acknowledgement::from_value(&Value::from(word)),
                None,
                "{word}"
            );
        }
        assert_eq!(Acknowledge::from_value(&Value::Null), None);
    }

    #[test]
    fn a_weaker_request_is_admitted_only_when_the_namespace_says_so() {
        let majority = Acknowledgement {
            level: Acknowledge::Majority,
            or_weaker: false,
        };
        assert!(majority.admits(Acknowledge::Majority));
        assert!(!majority.admits(Acknowledge::Leader));
        assert!(
            Acknowledgement {
                or_weaker: true,
                ..majority
            }
            .admits(Acknowledge::Leader)
        );
        let leader = Acknowledgement {
            level: Acknowledge::Leader,
            or_weaker: false,
        };
        assert!(
            leader.admits(Acknowledge::Majority),
            "asking for more is never refused"
        );
    }

    #[test]
    fn a_level_is_displayed_as_the_clause_that_states_it() {
        assert_eq!(Acknowledge::Majority.to_string(), "ACKNOWLEDGE MAJORITY");
        assert_eq!(
            Acknowledgement {
                level: Acknowledge::Majority,
                or_weaker: true
            }
            .to_string(),
            "ACKNOWLEDGE MAJORITY OR WEAKER"
        );
    }
}
