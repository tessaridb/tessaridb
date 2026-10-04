//! Breadth-first over every reachable state, checking as it goes.

use std::collections::{HashSet, VecDeque};

use super::state::{Coordinator, State, history};
use super::step::successors;
use super::{Decision, Entry, Range, Rules, Violation, World, Writer, check};

/// The first violation found, with the state that showed it.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) violation: Violation,
    pub(crate) state: String,
}

/// What a clean exploration covered. Each witness is a situation the
/// invariants are about; a run that never reached one would pass vacuously.
#[derive(Debug, Default)]
pub(crate) struct Explored {
    pub(crate) states: usize,
    /// A reader saw T1 on both keys.
    pub(crate) read_whole: bool,
    /// A run finished with T1 committed, and one with T1 aborted.
    pub(crate) committed: bool,
    pub(crate) aborted: bool,
    /// T2 committed on top of a committed T1.
    pub(crate) second_after: bool,
    /// A run forgot T1's record.
    pub(crate) forgotten: bool,
    /// A reader that began after the answer saw T1 whole at the leaders.
    pub(crate) acknowledged_seen: bool,
    /// The caller was told committed while the record still staged (D14).
    pub(crate) told_while_staging: bool,
    /// Status recovery committed a staging record, and barred a prepare.
    pub(crate) recovered_committed: bool,
    pub(crate) prevented: bool,
}

/// Every state reachable under `rules`, or the first one that breaks an
/// invariant.
pub(crate) fn explore(rules: Rules, world: World) -> Result<Explored, Found> {
    let start = State::initial();
    let mut known: HashSet<State> = HashSet::from([start.clone()]);
    let mut queue: VecDeque<State> = VecDeque::from([start]);
    let mut explored = Explored::default();
    while let Some(state) = queue.pop_front() {
        let found = |violation| Found {
            violation,
            state: format!("{state:?}"),
        };
        check::always(&state, rules).map_err(found)?;
        explored.read_whole |= state.reader.seen == [Some(Writer::T1); 2];
        explored.acknowledged_seen |= state.reader.began_after_answer == Some(true)
            && state.reader.at_leader == [true, true]
            && state.reader.seen == [Some(Writer::T1); 2];
        explored.told_while_staging |=
            state.told == Some(true) && state.record() == Some(Decision::Staging);
        explored.prevented |= state.log(Range::B).contains(&Entry::Prevent);
        let next = successors(&state, rules, world);
        // Recovery committed: a successor decided committed while the
        // coordinator had not concluded.
        explored.recovered_committed |= state.record() == Some(Decision::Staging)
            && !matches!(state.coordinator, Coordinator::Concluding)
            && next
                .iter()
                .any(|following| following.record() == Some(Decision::Committed));
        if next.is_empty() {
            check::finally(&state).map_err(found)?;
            let committed = state.outcome() == Some(Decision::Committed);
            explored.forgotten |= state.forgotten();
            explored.committed |= committed;
            explored.aborted |= !committed;
            explored.second_after |=
                history(state.log(Range::B), committed).ends_with(&[Writer::T1, Writer::T2]);
        }
        for following in next {
            if known.insert(following.clone()) {
                queue.push_back(following);
            }
        }
    }
    explored.states = known.len();
    Ok(explored)
}
