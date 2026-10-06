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
//! - **The caller's answer** (D14, parallel commit), told once every
//!   participant's prepare is held and the record is `STAGING` — before any
//!   decision is written. The coordinator's own range takes the staging record
//!   and its prepare in one commit, and the explicit decision with its own
//!   resolution in another, after the answer. A reader that starts after the
//!   answer, reading at the leaders, must see T1.
//! - **Status recovery** (D14), by anyone, at any time — a timeout is allowed
//!   to be early: a `STAGING` record whose every prepare is held is committed;
//!   one whose prepare at `B` is missing is aborted only after `B` has been
//!   barred from ever taking it.
//! - **The bar's end** (ADR-0119): `B`'s leader prunes its log past the bar
//!   at any moment, and the bar goes with it — from then on T1's prepare is
//!   refused because the log no longer reaches back to what T1 read (D3a).
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
    /// D4/D7: the record is begun by compare-and-set on *absent* and decided
    /// by compare-and-set on `STAGING`, so a participant that aborted an
    /// absent record and the coordinator's begin cannot both stand. Under D14
    /// the begin's half carries it: the decision follows from what is held.
    pub(crate) decide_by_compare_and_set: bool,
    /// D3: a participant prepares only if the version the transaction read is
    /// still the latest, and a prepare it already holds is answered, not
    /// written again.
    pub(crate) prepare_checks_the_stamp: bool,
    /// D5: an ordinary commit touching a key under a standing intent is refused.
    pub(crate) intent_refuses_writes: bool,
    /// D6/D14: a record not yet committed makes the transaction invisible —
    /// a `STAGING` one unless every participant's prepare is held, which is
    /// what committing implicitly means.
    pub(crate) undecided_is_invisible: bool,
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
    /// D13d: a reader meeting an intent asks the record's leader for T1's
    /// decision rather than reading its own copy of the record, which may not
    /// hold the decision yet when the caller was already told.
    pub(crate) readers_ask_the_record_leader: bool,
    /// D14: status recovery aborts a `STAGING` record only after barring the
    /// missing prepare at its participant, so an implicit commit and an abort
    /// cannot both happen.
    pub(crate) recovery_prevents_the_missing_prepare: bool,
    /// D3a, relied on by ADR-0119: a prepare whose read position the log no
    /// longer reaches is refused — what keeps a prepare out once the bar has
    /// been dropped with the record that wrote it.
    pub(crate) a_pruned_log_refuses_an_old_prepare: bool,
}

impl Rules {
    /// Every rule on: the protocol as ADR-0112 states it.
    pub(crate) const ALL: Self = Self {
        decide_by_compare_and_set: true,
        prepare_checks_the_stamp: true,
        intent_refuses_writes: true,
        undecided_is_invisible: true,
        one_decision_per_reader: true,
        visible_reads_its_own_version: true,
        restore_reads_by_the_snapshot: true,
        forget_waits_for_a_majority: true,
        readers_ask_the_record_leader: true,
        recovery_prevents_the_missing_prepare: true,
        a_pruned_log_refuses_an_old_prepare: true,
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
    /// Written with the coordinator's own prepare, naming every participant
    /// (D14): committed implicitly once every participant's prepare is held.
    Staging,
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
    /// Status recovery barred T1's prepare from ever landing in this range
    /// (`B` only, D14).
    Prevent,
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
    /// A finished run left a record staging or an intent standing.
    LeftInDoubt,
    /// The caller was told an outcome the record does not hold.
    ToldAgainstTheRecord,
    /// A reader that began after the caller was told T1 committed, reading
    /// every key at its leader, did not see T1 (D13).
    AcknowledgedUnseen,
    /// T1 was committed implicitly — its record `STAGING` and every prepare
    /// held — and its record was then decided aborted (D14).
    ImplicitCommitLost,
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
            explored.acknowledged_seen,
            "no reader that began after the answer read T1 at the leaders"
        );
        assert!(
            explored.committed && explored.aborted,
            "both outcomes must be reached"
        );
        assert!(explored.second_after, "T2 never committed on top of T1");
        // D14's situations: the caller told while the record still stages,
        // and recovery reaching each of its two outcomes.
        assert!(
            explored.told_while_staging,
            "the caller was never told before the decision was written"
        );
        assert!(
            explored.recovered_committed && explored.prevented,
            "status recovery never committed, or never barred a prepare"
        );
        // ADR-0119: a bar was dropped with its record, and a prepare arriving
        // after that was refused for reading what the log no longer holds.
        assert!(
            explored.bar_pruned && explored.refused_as_too_old,
            "no bar was ever pruned, or no prepare ever met a pruned log"
        );
        // The floor proves the interleavings were generated. It was 100 000
        // until D13 merged `A`'s record with its prepare and its decision with
        // its resolution (31 664 states measured then); D14's status recovery,
        // its barred prepare and the lost answer bring it to 97 537.
        assert!(
            explored.states > 90_000,
            "only {} states were explored",
            explored.states
        );
        Ok(())
    }

    #[test]
    fn every_rule_is_load_bearing() -> Result<(), String> {
        let cases: [Case; 11] = [
            (
                "decide by compare-and-set",
                |rules| rules.decide_by_compare_and_set = false,
                // Under D14 most often a begin staging over a record a
                // participant had already aborted.
                &[
                    Violation::TwoOutcomes,
                    Violation::ResolvedAgainstTheRecord,
                    Violation::ImplicitCommitLost,
                ],
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
                "an undecided record is invisible",
                |rules| rules.undecided_is_invisible = false,
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
            (
                "readers ask the record's leader",
                |rules| rules.readers_ask_the_record_leader = false,
                // Told at the decision, a participant's own copy of the record
                // may not hold it yet: the answer said committed, and a
                // reader at the leaders sees nothing of T1.
                &[Violation::AcknowledgedUnseen],
            ),
            (
                "recovery prevents the missing prepare",
                |rules| rules.recovery_prevents_the_missing_prepare = false,
                // A recovery that aborts while `B`'s prepare is still in
                // flight: the prepare lands, the caller is told committed, and
                // the record says aborted.
                // Or a backup cut holding the staging record and both parts,
                // restored as committed while the transaction aborted.
                &[
                    Violation::ToldAgainstTheRecord,
                    Violation::ImplicitCommitLost,
                    Violation::FracturedRestore,
                ],
            ),
            (
                "a pruned log refuses an old prepare",
                |rules| rules.a_pruned_log_refuses_an_old_prepare = false,
                // The bar dropped with its record and nothing in its place:
                // the prepare in flight lands after recovery barred it, so a
                // caller is told committed against an abort, or a cut shows
                // the staging record with both parts.
                &[
                    Violation::ToldAgainstTheRecord,
                    Violation::ImplicitCommitLost,
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
