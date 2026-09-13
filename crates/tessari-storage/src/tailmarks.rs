//! When this leader's log reached each position — G024 S6.1, Q-542.
//!
//! # What this exists to answer, and why the number beside it cannot
//!
//! [`crate::FollowerLag::quiet_for`] is how long since a follower last asked.
//! That is a real signal and it catches a failure no sequence count can see, but
//! it is not how old the follower's copy is, and on an idle leader the two
//! diverge completely: a follower that is perfectly level goes on reporting a
//! `quiet_for` that grows for as long as nothing happens.
//!
//! The age of a copy is a question about *this leader's own history* — when did
//! the log first hold something the follower does not. Nothing in the log can
//! answer it, because a record carries an epoch and mutations and no time. So
//! the leader keeps a short timeline of its own tail, and the follower's
//! position is read against it.
//!
//! # Why the cadence and not the commit path
//!
//! Sampling where the tail actually moves would be exact and would put a lock on
//! the hottest path in the engine, for a diagnostic. The awareness cadence
//! already runs once per `AWARENESS_SECONDS` and already opens the store, so the
//! sample rides something that was happening anyway and the commit path pays
//! nothing at all. The cost is precision, and it is bounded: see below.
//!
//! # The answer is an upper bound, by construction
//!
//! A mark says *the tail was at this position at this instant*. A follower
//! holding `S` is missing whatever came after the newest mark at or below `S`,
//! and that record was committed at some point between that mark and the next
//! one. Measuring from the earlier of the two overstates the age by at most one
//! cadence interval and never understates it — which refuses a copy that might
//! have been fine rather than serving one that might not be, the direction
//! [`crate::Lease`] errs in for the same reason.
//!
//! # Two halves of `mark`, neither of them an optimisation
//!
//! A sample that finds the tail unchanged **refreshes the newest mark in place**
//! rather than appending, and the reason is the window rather than the
//! arithmetic. Appending a duplicate would give the same answer — the reader
//! takes the newest mark at or below the follower's position, and among
//! duplicates that is the latest observation either way. What it would also do
//! is spend a slot: a leader idle for one window's worth of cadences would
//! evict its entire history in favour of copies of one position, and every
//! follower behind that position would become undatable while nothing at all
//! had happened. Refreshing in place keeps the latest observation of a position
//! without paying a slot for it, which is what holds the overstatement at one
//! cadence interval for as long as the window reaches back.
//!
//! # Why none of this is persisted
//!
//! The argument [`crate::followers`] and [`crate::running`] both make in their
//! own headers. A timeline of instants is a fact about a live process, and
//! `Instant` has no meaning across one: a restarted leader loading these marks
//! would publish ages computed from a clock that no longer exists.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tessari_types::Sequence;

/// How many samples of its own tail a leader keeps.
///
/// The window this can answer over is `TAIL_MARKS × AWARENESS_SECONDS`, so at
/// today's cadence a little over five minutes. A follower further behind than
/// that is answered `None` — *older than this leader has sampled* — rather than
/// with a saturated number, which is the distinction
/// [`crate::Store::current_as_of`] already draws between a bound that is known
/// to be exceeded and one nothing can state.
const TAIL_MARKS: usize = 32;

/// A short timeline of the positions this leader's log has reached.
///
/// Shared by every handle to one store, for the reason the follower registry is:
/// two handles are not two leaders, and a tail dated against one of them is a
/// tail the other cannot read.
#[derive(Debug, Default)]
pub struct TailMarks {
    /// Oldest first. Strictly increasing in sequence, non-decreasing in instant.
    marks: Mutex<VecDeque<(Sequence, Instant)>>,
}

impl TailMarks {
    /// Record that the committed tail stands at `tail`, as of now.
    ///
    /// Takes no result and returns none, for the reason
    /// [`crate::followers::Followers::served`] does: a timeline that can refuse
    /// is a timeline that can fail something real in order to protect a
    /// diagnostic.
    pub fn mark(&self, tail: Sequence) {
        let Ok(mut marks) = self.marks.lock() else {
            return;
        };
        let now = Instant::now();
        match marks.back_mut() {
            // The tail has not moved: this is the same position, seen later.
            // Re-dating it rather than appending is what stops an idle leader
            // from spending its whole window on copies of one position.
            Some((newest, at)) if *newest >= tail => *at = now,
            _ => {
                if marks.len() == TAIL_MARKS {
                    marks.pop_front();
                }
                marks.push_back((tail, now));
            }
        }
    }

    /// How old a copy holding everything up to `held` is, if that can be said.
    ///
    /// `None` means the copy predates every mark this leader kept — the honest
    /// answer, and one a caller must treat as beyond every bound.
    #[must_use]
    pub fn age_of(&self, held: Sequence) -> Option<Duration> {
        let marks = self.marks.lock().ok()?;
        marks
            .iter()
            .rev()
            .find(|(sequence, _)| *sequence <= held)
            .map(|(_, at)| at.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::{TAIL_MARKS, TailMarks};
    use tessari_types::Sequence;

    fn sequence(value: u64) -> Sequence {
        Sequence::new(value)
    }

    #[test]
    fn a_leader_that_has_marked_nothing_can_date_no_copy_at_all() {
        assert!(TailMarks::default().age_of(sequence(1)).is_none());
    }

    #[test]
    fn a_copy_level_with_the_newest_mark_is_dated_from_that_mark() {
        let marks = TailMarks::default();
        marks.mark(sequence(10));
        assert!(marks.age_of(sequence(10)).is_some());
        assert!(marks.age_of(sequence(11)).is_some());
    }

    #[test]
    fn a_copy_older_than_every_mark_has_no_age_to_state() {
        let marks = TailMarks::default();
        marks.mark(sequence(10));
        marks.mark(sequence(20));
        assert!(marks.age_of(sequence(9)).is_none());
    }

    #[test]
    fn an_unmoved_tail_is_redated_rather_than_appended() {
        let marks = TailMarks::default();
        marks.mark(sequence(10));
        let first = marks.age_of(sequence(10)).expect("a mark was made");
        marks.mark(sequence(10));
        let second = marks.age_of(sequence(10)).expect("the mark is still there");
        assert!(
            second <= first,
            "re-marking the same tail must date it later, not earlier: {second:?} vs {first:?}"
        );
        assert_eq!(
            marks.marks.lock().expect("not poisoned").len(),
            1,
            "an unmoved tail must not consume a second slot"
        );
    }

    #[test]
    fn the_window_is_bounded_and_the_oldest_marks_go_first() {
        let marks = TailMarks::default();
        for position in 1..=u64::try_from(TAIL_MARKS).expect("fits") + 5 {
            marks.mark(sequence(position));
        }
        assert_eq!(marks.marks.lock().expect("not poisoned").len(), TAIL_MARKS);
        assert!(
            marks.age_of(sequence(1)).is_none(),
            "a position evicted from the window can no longer be dated"
        );
    }
}
