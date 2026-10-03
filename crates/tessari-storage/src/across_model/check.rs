//! The invariants ADR-0112 promises, asked of one state.

use super::state::{Second, Shown, State, history, intent_stands, record_in, versions};
use super::{Decision, Entry, Range, Rules, Violation, Writer};

/// What must hold in every reachable state.
pub(super) fn always(state: &State, rules: Rules) -> Result<(), Violation> {
    if state.two_outcomes() {
        return Err(Violation::TwoOutcomes);
    }
    let seen_t1 = state.reader.seen.contains(&Some(Writer::T1));
    if seen_t1 && state.record() == Some(Decision::Aborted) {
        return Err(Violation::DirtyRead);
    }
    if let [Some(a), Some(b)] = state.reader.seen
        && !atomic(state, [a, b])
    {
        return Err(Violation::FracturedRead);
    }
    // A backup could be taken at any lag of either copy, so every pair of
    // prefixes is a cut some whole holder could hold right now.
    for a in 1..=state.log(Range::A).len() {
        for b in 1..=state.log(Range::B).len() {
            if !atomic(state, restored(state, rules, [a, b])) {
                return Err(Violation::FracturedRestore);
            }
        }
    }
    Ok(())
}

/// What must hold once nothing more can happen.
pub(super) fn finally(state: &State) -> Result<(), Violation> {
    let committed = match state.outcome() {
        Some(Decision::Committed) => true,
        Some(Decision::Aborted) => false,
        Some(Decision::Pending) | None => return Err(Violation::LeftInDoubt),
    };
    for range in Range::BOTH {
        let log = state.log(range);
        if intent_stands(log) {
            return Err(Violation::LeftInDoubt);
        }
        let disagrees = log.iter().any(
            |entry| matches!(entry, Entry::Resolved { committed: resolved } if *resolved != committed),
        );
        if disagrees {
            return Err(Violation::ResolvedAgainstTheRecord);
        }
        // First committer wins: every committed writer read the version it
        // replaced — T1 read the initial versions, T2 whatever it was told.
        let written = history(log, committed);
        for pair in written.windows(2) {
            let read = match pair[1] {
                Writer::T1 => Writer::Initial,
                Writer::T2 => match state.second {
                    Second::Done { stamp } => stamp,
                    Second::Idle | Second::Read { .. } => return Err(Violation::LostUpdate),
                },
                Writer::Initial => return Err(Violation::LostUpdate),
            };
            if read != pair[0] {
                return Err(Violation::LostUpdate);
            }
        }
    }
    Ok(())
}

/// Read atomic: whoever saw a version of T1 sees, for every key T1 wrote,
/// T1's version or one written after it.
fn atomic(state: &State, seen: [Writer; 2]) -> bool {
    if !seen.contains(&Writer::T1) {
        return true;
    }
    Range::BOTH
        .iter()
        .zip(seen)
        .all(|(range, writer)| match writer {
            Writer::T1 => true,
            Writer::Initial => false,
            Writer::T2 => {
                let shown = versions(state.log(*range));
                let t1 = shown
                    .iter()
                    .position(|version| matches!(version, Shown::Intent | Shown::Resolved));
                let t2 = shown
                    .iter()
                    .position(|version| *version == Shown::Plain(Writer::T2));
                matches!((t1, t2), (Some(t1), Some(t2)) if t2 > t1)
            }
        })
}

/// What a restore of a backup taken by the lagging node right now would hold.
///
/// D9a: the backup carries the cut as it stands, and the restore decides as a
/// reader of that cut does (D6a) — T1 is shown only where the cut knows it
/// committed and holds its part in both ranges. Without the rule each range
/// restores whatever its own prefix says, which is what a copy of the newest
/// versions did.
fn restored(state: &State, rules: Rules, applied: [usize; 2]) -> [Writer; 2] {
    let prefix = |range: Range| &state.log(range)[..applied[range.slot()]];
    let committed = record_in(prefix(Range::A)) == Some(Decision::Committed)
        || Range::BOTH
            .iter()
            .any(|range| prefix(*range).contains(&Entry::Resolved { committed: true }));
    let landed = Range::BOTH.iter().all(|range| {
        prefix(*range)
            .iter()
            .any(|entry| matches!(entry, Entry::Intent { .. }))
    });
    let shown = committed && (landed || !rules.restore_reads_by_the_snapshot);
    let mut seen = [Writer::Initial; 2];
    for range in Range::BOTH {
        seen[range.slot()] = versions(prefix(range))
            .iter()
            .rev()
            .find_map(|version| match version {
                Shown::Plain(writer) => Some(*writer),
                Shown::Resolved | Shown::Intent => shown.then_some(Writer::T1),
            })
            .unwrap_or(Writer::Initial);
    }
    seen
}
