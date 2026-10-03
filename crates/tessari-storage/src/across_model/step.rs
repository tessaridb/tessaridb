//! Every action any actor can take from a state — the interleavings.

use super::state::{
    Coordinator, Second, Shown, State, holds_intent, intent_stands, latest_committed, versions,
};
use super::{Decision, Entry, Range, Rules, World, Writer};

/// How many times one range may be handed T1's prepare: once, and once more
/// as a duplicate.
const DELIVERIES: u8 = 2;

/// Every state one action away from `state`.
pub(super) fn successors(state: &State, rules: Rules, world: World) -> Vec<State> {
    let mut next = Vec::new();
    coordinate(state, rules, &mut next);
    for range in Range::BOTH {
        deliver(state, rules, range, &mut next);
        resolve(state, range, world, &mut next);
        if world == World::Reading {
            read(state, rules, range, &mut next);
        }
    }
    lapse(state, &mut next);
    match world {
        World::Reading => second(state, rules, &mut next),
        World::Forgetting => {
            settle_absent(state, &mut next);
            replicate(state, &mut next);
            forget(state, rules, &mut next);
        }
    }
    next
}

fn coordinate(state: &State, rules: Rules, next: &mut Vec<State>) {
    match &state.coordinator {
        Coordinator::Idle => {
            // D4: the record is written PENDING before any prepare leaves.
            let mut begun = state.clone();
            begun.logs[Range::A.slot()].push(Entry::Record(Decision::Pending));
            begun.prepares = [1, 1];
            begun.coordinator = Coordinator::Waiting {
                replies: [None, None],
            };
            next.push(begun);
        }
        Coordinator::Waiting { replies } => {
            for range in Range::BOTH {
                if let Some(answer) = state.replies[range.slot()] {
                    let mut heard = state.clone();
                    heard.replies[range.slot()] = None;
                    if let Coordinator::Waiting { replies } = &mut heard.coordinator {
                        replies[range.slot()] = Some(answer);
                    }
                    next.push(heard);
                }
            }
            let outcome = if replies.iter().all(|reply| *reply == Some(true)) {
                Some(Decision::Committed)
            } else if replies.contains(&Some(false)) {
                Some(Decision::Aborted)
            } else {
                None
            };
            if let Some(outcome) = outcome {
                let mut decided = state.clone();
                decide(&mut decided, rules, outcome);
                decided.coordinator = Coordinator::Done;
                next.push(decided);
            }
            let mut crashed = state.clone();
            crashed.coordinator = Coordinator::Crashed;
            next.push(crashed);
        }
        Coordinator::Done | Coordinator::Crashed => {}
    }
}

/// Record a decision — by compare-and-set on `PENDING` when the rule holds.
fn decide(state: &mut State, rules: Rules, outcome: Decision) {
    let pending = state.record() == Some(Decision::Pending);
    if pending || !rules.decide_by_compare_and_set {
        state.logs[Range::A.slot()].push(Entry::Record(outcome));
    }
}

/// D7: a record whose liveness lapsed is aborted — possibly while the
/// coordinator still lives, because a timeout is allowed to be early.
fn lapse(state: &State, next: &mut Vec<State>) {
    if state.record() == Some(Decision::Pending) {
        let mut lapsed = state.clone();
        lapsed.logs[Range::A.slot()].push(Entry::Record(Decision::Aborted));
        next.push(lapsed);
    }
}

/// A participant receives T1's prepare: consumed, or kept to arrive again.
fn deliver(state: &State, rules: Rules, range: Range, next: &mut Vec<State>) {
    let slot = range.slot();
    if state.prepares[slot] == 0 || state.deliveries[slot] >= DELIVERIES {
        return;
    }
    for duplicate in [false, true] {
        let mut delivered = state.clone();
        delivered.deliveries[slot] = delivered.deliveries[slot].saturating_add(1);
        if !duplicate {
            delivered.prepares[slot] = 0;
        }
        let log = delivered.log(range).to_vec();
        // T1 read the initial version of both keys.
        let valid = latest_committed(&log) == Writer::Initial && !intent_stands(&log);
        let answer = if rules.prepare_checks_the_stamp {
            if holds_intent(&log) && intent_stands(&log) {
                // Already prepared: answered, not written again.
                true
            } else if valid {
                delivered.logs[slot].push(Entry::Intent { valid });
                true
            } else {
                false
            }
        } else {
            delivered.logs[slot].push(Entry::Intent { valid });
            true
        };
        delivered.replies[slot] = Some(answer);
        next.push(delivered);
    }
}

/// Anyone turns a standing intent into a version or drops it, once the record
/// has decided. Idempotent: a resolved intent no longer stands. `B`'s leader
/// applies it alone first — a resolution waits for no majority.
fn resolve(state: &State, range: Range, world: World, next: &mut Vec<State>) {
    let decided = match state.record() {
        Some(Decision::Committed) => true,
        Some(Decision::Aborted) => false,
        Some(Decision::Pending) | None => return,
    };
    if state.intent_stands_at_leader(range) {
        let mut resolved = state.clone();
        match (range, world) {
            (Range::B, World::Forgetting) => resolved.tail = Some(decided),
            _ => resolved.logs[range.slot()].push(Entry::Resolved { committed: decided }),
        }
        next.push(resolved);
    }
}

/// `B`'s leader-only resolution reaches a majority — or its leader dies first
/// and the resolution is gone with it.
fn replicate(state: &State, next: &mut Vec<State>) {
    if let Some(committed) = state.tail {
        let mut held = state.clone();
        held.logs[Range::B.slot()].push(Entry::Resolved { committed });
        held.tail = None;
        next.push(held);
        let mut lost = state.clone();
        lost.tail = None;
        next.push(lost);
    }
}

/// D7: a participant holding an intent whose record is absent aborts it, by
/// compare-and-set on *absent* at the record's leader.
fn settle_absent(state: &State, next: &mut Vec<State>) {
    let standing = Range::BOTH
        .iter()
        .any(|range| state.intent_stands_at_leader(*range));
    if standing && state.record().is_none() && state.coordinator != Coordinator::Idle {
        let mut aborted = state.clone();
        aborted.logs[Range::A.slot()].push(Entry::Record(Decision::Aborted));
        next.push(aborted);
    }
}

/// D12: `A`'s leader forgets a decided record once every participant says
/// none of its intents stands — said, under the rule, only once a majority
/// holds the resolution that removed them.
fn forget(state: &State, rules: Rules, next: &mut Vec<State>) {
    if !matches!(
        state.record(),
        Some(Decision::Committed | Decision::Aborted)
    ) {
        return;
    }
    let gone = Range::BOTH.iter().all(|range| {
        !state.intent_stands_at_leader(*range)
            && (!rules.forget_waits_for_a_majority || *range == Range::A || state.tail.is_none())
    });
    if gone {
        let mut forgot = state.clone();
        forgot.logs[Range::A.slot()].push(Entry::Forget);
        next.push(forgot);
    }
}

/// T2 reads key `b` at its leader, then commits against what it read.
fn second(state: &State, rules: Rules, next: &mut Vec<State>) {
    let log = state.log(Range::B);
    match state.second {
        Second::Idle => {
            let mut read = state.clone();
            read.second = Second::Read {
                stamp: latest_committed(log),
            };
            next.push(read);
        }
        Second::Read { stamp } => {
            let blocked = rules.intent_refuses_writes
                && intent_stands(log)
                && state.record() != Some(Decision::Aborted);
            let mut done = state.clone();
            if !blocked && latest_committed(log) == stamp {
                done.logs[Range::B.slot()].push(Entry::Version(Writer::T2));
            }
            done.second = Second::Done { stamp };
            next.push(done);
        }
        Second::Done { .. } => {}
    }
}

/// The reading transaction reads one key it has not read yet (D6).
fn read(state: &State, rules: Rules, range: Range, next: &mut Vec<State>) {
    let slot = range.slot();
    if state.reader.seen[slot].is_some() {
        return;
    }
    // The lagging node has applied some prefix of this range's log — any
    // prefix, since nothing ties its lag to anything else — and the read sees
    // that prefix. Every choice is a successor.
    for applied in 1..=state.log(range).len() {
        next.push(read_at(state, rules, range, applied));
    }
}

fn read_at(state: &State, rules: Rules, range: Range, applied: usize) -> State {
    let slot = range.slot();
    let mut after = state.clone();
    let copy = &state.log(range)[..applied];
    let other_read = state.reader.seen[range.other().slot()].is_some();
    let mut seen = None;
    for version in versions(copy).iter().rev() {
        let committed_known = match version {
            Shown::Plain(writer) => {
                seen = Some(*writer);
                break;
            }
            Shown::Resolved => true,
            // Asked of the record's leader over the peer link.
            Shown::Intent => match state.record() {
                Some(Decision::Committed) => true,
                Some(Decision::Pending) => !rules.pending_is_invisible,
                Some(Decision::Aborted) | None => false,
            },
        };
        let visible = match after.reader.decided {
            Some(visible) if rules.one_decision_per_reader => visible,
            _ => {
                let visible = committed_known && !(rules.one_decision_per_reader && other_read);
                if rules.one_decision_per_reader {
                    after.reader.decided = Some(visible);
                }
                visible
            }
        };
        if visible {
            seen = Some(Writer::T1);
            break;
        }
    }
    let mut seen = seen.unwrap_or(Writer::Initial);
    // D6: a visible T1 is read at its own version even where this copy has not
    // applied the prepare yet — fetched by T1's id from the leader, which holds
    // it, since a committed record means every participant prepared.
    if rules.visible_reads_its_own_version
        && after.reader.decided == Some(true)
        && !holds_intent(copy)
        && holds_intent(state.log(range))
    {
        seen = Writer::T1;
    }
    after.reader.seen[slot] = Some(seen);
    after
}
