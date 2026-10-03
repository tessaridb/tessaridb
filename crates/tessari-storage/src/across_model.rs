//! The commit across leaders, as a model checked in every reachable state
//! (ADR-0112 D10) — written before the engine code it governs.
//!
//! Two ranges, each holding one key: `A` (which also holds the transaction's
//! record, so its leader coordinates — D2) and `B`. A log here is what a
//! majority holds: ADR-0106 proves separately that an entry held by a majority
//! survives any leader change, so a leader change in this model is the loss of
//! the coordinator's volatile state and nothing else. What the model adds on
//! top is everything that can interleave:
//!
//! - **T1**, the cross-leader transaction, writing both keys from the initial
//!   versions it read; its coordinator can crash at any point, its prepare
//!   messages can be delivered twice, and its record can be aborted by a lapse
//!   while the coordinator is still alive (a timeout is allowed to be early).
//! - **T2**, an ordinary single-range commit of key `b`, racing T1.
//! - **A reading transaction** on a node whose copies of both logs lag their
//!   leaders independently, reading the two keys in either order.
//! - **A backup** taken by that node at every reachable moment (D9a).
//! - **Forgetting** the decided record once every participant says its
//!   intents are gone (D12), while `B`'s leader may hold a resolution no
//!   majority holds yet and die with it — a resolution waits for its leader
//!   alone.
//!
//! Each rule the ADR relies on is a switch in [`Rules`]. The protocol is
//! checked with all of them on; then each is turned off alone and the explorer
//! must find the violation that rule exists to prevent. A rule whose removal
//! breaks nothing is either not needed or not modelled, and both are findings.

mod check;
mod explore;
mod state;
mod step;

pub(crate) use explore::explore;

/// Which world is explored. Each is the whole protocol; they differ in which
/// actors interleave with it, because both at once is eleven times the states
/// for no interleaving either needs from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum World {
    /// The reader on a lagging node and T2 racing T1 (D3-D6, D9).
    Reading,
    /// The record forgotten while `B`'s leader may die holding the only copy
    /// of its resolution, and backups cut across it (D7, D9, D12).
    Forgetting,
}

/// The rules ADR-0112 relies on, each removable to prove it is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rules {
    /// D4/D7: a decision is written by compare-and-set on `PENDING`, so the
    /// coordinator and a lapse cannot both record one.
    pub(crate) decide_by_compare_and_set: bool,
    /// D3: a participant prepares only if the version the transaction read is
    /// still the latest, and a prepare it already holds is answered, not
    /// written again.
    pub(crate) prepare_checks_the_stamp: bool,
    /// D5: an ordinary commit touching a key under a standing intent is refused.
    pub(crate) intent_refuses_writes: bool,
    /// D6: a `PENDING` record makes the transaction invisible.
    pub(crate) pending_is_invisible: bool,
    /// D6: one visibility decision per reading transaction, *invisible* when
    /// another participant range has already been read.
    pub(crate) one_decision_per_reader: bool,
    /// D6: a visible transaction is read from a copy holding its prepare, or
    /// from the leader — never from a copy behind it.
    pub(crate) visible_reads_its_own_version: bool,
    /// D9a: a backup carries its cut as it stands, and its restore shows T1
    /// only where that cut knows T1 committed and holds both its parts — the
    /// reading rule of D6a applied to the restored copy.
    pub(crate) restore_reads_by_the_snapshot: bool,
    /// D12: a participant says its intents are gone only once a majority
    /// holds its resolution, so a successor cannot find one standing.
    pub(crate) forget_waits_for_a_majority: bool,
}

impl Rules {
    /// Every rule on: the protocol as ADR-0112 states it.
    pub(crate) const ALL: Self = Self {
        decide_by_compare_and_set: true,
        prepare_checks_the_stamp: true,
        intent_refuses_writes: true,
        pending_is_invisible: true,
        one_decision_per_reader: true,
        visible_reads_its_own_version: true,
        restore_reads_by_the_snapshot: true,
        forget_waits_for_a_majority: true,
    };
}

/// The two ranges, each holding one key of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Range {
    /// Holds key `a` and T1's record.
    A,
    /// Holds key `b`.
    B,
}

impl Range {
    const BOTH: [Self; 2] = [Self::A, Self::B];

    const fn slot(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// Who wrote a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Writer {
    /// The version every key starts with.
    Initial,
    /// The cross-leader transaction.
    T1,
    /// The single-range commit of key `b`.
    T2,
}

/// The state T1's record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Decision {
    /// Written before the first prepare is sent.
    Pending,
    /// Every participant prepared.
    Committed,
    /// A participant refused, or the record's liveness lapsed.
    Aborted,
}

/// One entry of a range's log, as held by a majority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    /// An ordinary committed version of the range's key.
    Version(Writer),
    /// T1's provisional value of the range's key. `valid` is the model's own
    /// bookkeeping — whether the version T1 read was still the latest when
    /// this was written — and is never consulted by the protocol.
    Intent { valid: bool },
    /// T1's intent turned into a version, or dropped.
    Resolved { committed: bool },
    /// A change of T1's record (range `A` only).
    Record(Decision),
    /// T1's record deleted, its outcome settled everywhere (range `A`, D12).
    Forget,
}

/// What an invariant check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Violation {
    /// More than one decision was recorded.
    TwoOutcomes,
    /// A resolution disagrees with the record's final decision.
    ResolvedAgainstTheRecord,
    /// A committed writer did not read the version it replaced.
    LostUpdate,
    /// A reader saw a version of a transaction that did not commit.
    DirtyRead,
    /// A reader saw part of T1.
    FracturedRead,
    /// A restore of a backup shows part of T1.
    FracturedRestore,
    /// A finished run left a record pending or an intent standing.
    LeftInDoubt,
}

#[cfg(test)]
mod tests {
    use super::{Rules, Violation, World, explore};

    /// A rule's name, how to remove it, and what its removal may break.
    type Case = (&'static str, fn(&mut Rules), &'static [Violation]);

    #[test]
    fn the_protocol_holds_every_invariant_in_every_reachable_state() -> Result<(), String> {
        let violated = |found: super::explore::Found| {
            format!(
                "ADR-0112 as stated is violated: {:?} in {}",
                found.violation, found.state
            )
        };
        let forgetting = explore(Rules::ALL, World::Forgetting).map_err(violated)?;
        assert!(forgetting.forgotten, "no run ever forgot T1's record");
        assert!(
            forgetting.committed && forgetting.aborted,
            "both outcomes must be forgotten from"
        );
        let explored = explore(Rules::ALL, World::Reading).map_err(violated)?;
        // The situations the invariants are about were all reached, so no
        // invariant held vacuously; and the floor proves the interleavings
        // were generated at all.
        assert!(explored.read_whole, "no reader ever saw T1 whole");
        assert!(
            explored.committed && explored.aborted,
            "both outcomes must be reached"
        );
        assert!(explored.second_after, "T2 never committed on top of T1");
        assert!(
            explored.states > 100_000,
            "only {} states were explored",
            explored.states
        );
        Ok(())
    }

    #[test]
    fn every_rule_is_load_bearing() -> Result<(), String> {
        let cases: [Case; 8] = [
            (
                "decide by compare-and-set",
                |rules| rules.decide_by_compare_and_set = false,
                &[Violation::TwoOutcomes, Violation::ResolvedAgainstTheRecord],
            ),
            (
                "prepare checks the stamp",
                |rules| rules.prepare_checks_the_stamp = false,
                // And D12's: a duplicate prepare arriving after a committed
                // record was forgotten would stand an intent that settles
                // against nothing and aborts.
                &[Violation::LostUpdate, Violation::TwoOutcomes],
            ),
            (
                "an intent refuses writes",
                |rules| rules.intent_refuses_writes = false,
                &[Violation::LostUpdate],
            ),
            (
                "pending is invisible",
                |rules| rules.pending_is_invisible = false,
                &[Violation::DirtyRead, Violation::FracturedRead],
            ),
            (
                "one decision per reader",
                |rules| rules.one_decision_per_reader = false,
                &[Violation::FracturedRead],
            ),
            (
                "a visible read reads its own version",
                |rules| rules.visible_reads_its_own_version = false,
                &[Violation::FracturedRead],
            ),
            (
                "a restore reads by the snapshot",
                |rules| rules.restore_reads_by_the_snapshot = false,
                &[Violation::FracturedRestore],
            ),
            (
                "forget waits for a majority",
                |rules| rules.forget_waits_for_a_majority = false,
                // The same hazard seen twice: a successor settling an intent
                // against an absent record, or a backup cut past the Forget
                // that has no resolution of `B` to reach and drops its intent.
                &[
                    Violation::TwoOutcomes,
                    Violation::ResolvedAgainstTheRecord,
                    Violation::FracturedRestore,
                ],
            ),
        ];
        for (name, remove, expected) in cases {
            let mut rules = Rules::ALL;
            remove(&mut rules);
            let Some(found) = [World::Reading, World::Forgetting]
                .into_iter()
                .find_map(|world| explore(rules, world).err())
            else {
                return Err(format!("without `{name}` nothing broke"));
            };
            assert!(
                expected.contains(&found.violation),
                "without `{name}` the first violation was {:?}, expected one of {expected:?}, in {}",
                found.violation,
                found.state
            );
        }
        Ok(())
    }
}
