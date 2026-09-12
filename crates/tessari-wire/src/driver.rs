//! The three cadences a node runs, and why each gets a thread of its own.
//!
//! # Three cadences, three threads
//!
//! A node that has joined a cluster has three things to do on a timer: greet its
//! peers so it knows how current each one is, collect records from whoever it
//! follows, and renew the lease its leadership rests on. They look alike enough
//! to fold into one loop, and folding them is the mistake.
//!
//! They fail differently. A missed greeting costs the *freshness of a reading* —
//! routing gets more conservative, which is the direction it should fail in. A
//! missed collection costs *data*, and the node simply falls further behind. A
//! missed renewal costs *leadership*, and the fence closes whether or not anyone
//! noticed.
//!
//! One loop gives all three a single period, a single failure path, and a single
//! thread's fate. The sharpest consequence is the last: `collect` and `renew`
//! both dial peers, so a collection blocked on a dead peer's TCP connect would
//! hold up a renewal whose fence is closing. The cadence with the tightest
//! deadline would be delayed by the one with the loosest, for no reason beyond
//! their sharing a thread.
//!
//! # Nothing here hands a `Result` to a timer
//!
//! Each cadence answers *what it did* — a cursor, a lease — rather than whether
//! it went well. The caller is a loop that runs again either way, so an error
//! return would only ever be dropped, and a dropped error reads at the call site
//! as if failure were impossible. What each driver owns instead is the rule for
//! **what a failed pass does to the state it holds**, which is the part that is
//! genuinely easy to get wrong.
//!
//! # The pass is a parameter
//!
//! Every driver takes the work as a closure, the way [`crate::Directory`] takes
//! its clock and its greeting. A driver that dialled a socket itself could only
//! be tested by standing up peers, and one that read the clock itself could only
//! have its timing rule tested by waiting.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_serve::Stopping;
use tessari_storage::Lease;
use tessari_types::{Epoch, Sequence};

use crate::directory::Directory;
use crate::grant::Leadership;

/// How long to wait before the next pass, given when the last one started.
///
/// # A missed tick is not made up
///
/// When a pass overran its period — the peer was slow, the thread was
/// descheduled — this answers [`Duration::ZERO`] and the next pass runs at once.
/// It never answers *run it three times because three periods went by*.
///
/// Catching up would be wrong for each cadence separately. Three greeting rounds
/// back to back dial every peer three times to learn one thing. Three
/// collections are unnecessary because the cursor already carries the position,
/// so one pass fetches as much as the peer's limit allows regardless of how long
/// it has been. And three renewals are three elections where the cluster needed
/// none.
#[must_use]
pub fn due_in(period: Duration, ran_at: Instant, now: Instant) -> Duration {
    period.saturating_sub(now.saturating_duration_since(ran_at))
}

/// Run `pass` on `period` until the node is asked to stop.
///
/// The flag is checked **before** each pass, so a node already stopping runs
/// none. Stopping during the wait takes effect at the end of it: a cadence is
/// not on the shutdown path, and waking it early would buy a fraction of a
/// period at the cost of a second way to interrupt a thread.
///
/// The stop flag is the node's own [`Stopping`] rather than one of this
/// module's. A driver with a private flag gives a process two ways to ask a node
/// to stop, and the state between them — a node that has stopped serving while
/// it goes on dialling peers — is worse than either.
pub fn every(period: Duration, stopping: &Stopping, mut pass: impl FnMut(Instant)) {
    while !stopping.asked() {
        let ran_at = Instant::now();
        pass(ran_at);
        std::thread::sleep(due_in(period, ran_at, Instant::now()));
    }
}

/// The directory the routing side reads, and the greeting side replaces.
///
/// # Why a copy and a swap rather than a lock held across the round
///
/// [`Directory::greet_round`] takes `&mut self` and dials each peer *inside* the
/// walk, so a shared `Mutex<Directory>` would hold the lock for the length of
/// every connection attempt. Every routing read would then wait on the slowest
/// unreachable peer in the cluster — which is exactly the node the directory
/// exists to route around, so the structure would turn one node's failure into
/// every reader's latency.
///
/// Instead the greeting side takes a copy, dials into the copy with no lock
/// held, and swaps the result in under a lock held for the swap alone. Readers
/// see the previous round's answers until the new ones are all in, which is a
/// consistent view rather than a partial one.
///
/// The copy is taken from the *current* directory and not from an empty one, so
/// a peer that was heard two rounds ago and has been silent since is carried
/// forward and goes on ageing. That is what makes W234's rule survive the swap:
/// a silent peer grows old, and starting each round from nothing would instead
/// make every silent peer vanish once per period.
#[derive(Debug)]
pub struct Published {
    current: Mutex<Arc<Directory>>,
}

impl Published {
    /// Publish `directory` as the current answer.
    #[must_use]
    pub fn holding(directory: Directory) -> Self {
        Self {
            current: Mutex::new(Arc::new(directory)),
        }
    }

    /// The directory as it stands.
    ///
    /// The lock is held only long enough to clone a pointer, so a reader never
    /// waits on a greeting round.
    #[must_use]
    pub fn current(&self) -> Arc<Directory> {
        Arc::clone(&self.held())
    }

    /// Run one greeting round against a copy, then publish it.
    pub fn round(&self, greet: impl FnOnce(&mut Directory)) {
        let mut next = (*self.current()).clone();
        greet(&mut next);
        *self.held() = Arc::new(next);
    }

    /// The guard, recovering rather than panicking if a holder died mid-swap.
    ///
    /// What the lock protects is one pointer. A thread that panicked while
    /// holding it left a whole directory behind, never half of one, so the value
    /// is sound and refusing to read it would take routing down over an
    /// unrelated failure elsewhere.
    fn held(&self) -> std::sync::MutexGuard<'_, Arc<Directory>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How far this node has collected, and what a failed collection does to it.
#[derive(Debug)]
pub struct Collecting {
    at: Sequence,
}

impl Collecting {
    /// Start collecting from `at` — the first position this node does not hold.
    #[must_use]
    pub fn from(at: Sequence) -> Self {
        Self { at }
    }

    /// The position this node has reached.
    #[must_use]
    pub fn reached(&self) -> Sequence {
        self.at
    }

    /// One collection. The cursor moves **only** when the pass answers.
    ///
    /// # Why a failure leaves the cursor alone
    ///
    /// A cursor advanced past records that were never applied skips them
    /// permanently and silently: the next pass asks for what comes after, no
    /// later pass ever asks for the gap, and nothing is in an error state to say
    /// so. Leaving it costs a repeated request when the peer comes back, which
    /// is the failure worth having.
    pub fn once<E>(&mut self, pass: impl FnOnce(Sequence) -> Result<Sequence, E>) -> Sequence {
        if let Ok(reached) = pass(self.at) {
            self.at = reached;
        }
        self.at
    }
}

/// The leadership this node holds, and what a lost round does to it.
#[derive(Debug)]
pub struct Renewing {
    standing: Leadership,
}

impl Renewing {
    /// Hold `standing` until something better is won.
    #[must_use]
    pub fn holding(standing: Leadership) -> Self {
        Self { standing }
    }

    /// The leadership this node is currently standing on.
    #[must_use]
    pub fn standing(&self) -> Leadership {
        self.standing
    }

    /// One renewal round, standing for the epoch after the one held.
    ///
    /// # Why a round that wins nothing changes nothing
    ///
    /// [`crate::Standing::renew`] answers `None` in two circumstances: there was
    /// margin left and nobody was asked, or a round ran and no majority granted
    /// it. Both mean *carry on with the lease you have*, and the first is the
    /// ordinary case — a cadence that ran a little early. Standing down on
    /// `None` would make a healthy leader resign because its timer fired before
    /// its fence needed defending.
    pub fn once(&mut self, pass: impl FnOnce(Lease, Epoch) -> Option<Leadership>) -> Leadership {
        let next = Epoch::new(self.standing.epoch.get().saturating_add(1));
        if let Some(won) = pass(self.standing.lease(), next) {
            self.standing = won;
        }
        self.standing
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
    use tessari_serve::Stopping;
    use tessari_storage::Lease;
    use tessari_types::{Epoch, Sequence};

    use super::{Collecting, Published, Renewing, due_in, every};
    use crate::directory::Directory;
    use crate::grant::Leadership;
    use crate::peer::Hello;

    const NODE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];

    /// A serving peer one second behind.
    fn said() -> Hello {
        Hello {
            node: NODE,
            build: NodeVersion {
                major: 0,
                minor: 1,
                patch: 1,
            },
            epoch: Epoch::new(7),
            roles: Roles::SERVING,
            tail: Sequence::new(4096),
            current_as_of: Some(Duration::from_secs(1)),
        }
    }

    #[test]
    fn a_failed_collection_retries_from_the_same_position() {
        let mut collecting = Collecting::from(Sequence::new(5));
        let reached = collecting.once(|_| Err::<Sequence, ()>(()));
        assert_eq!(reached, Sequence::new(5), "a failed pass moved the cursor");
        assert_eq!(collecting.reached(), Sequence::new(5));

        let asked = RefCell::new(Vec::new());
        let reached = collecting.once(|at| {
            asked.borrow_mut().push(at);
            Ok::<Sequence, ()>(Sequence::new(at.get() + 10))
        });
        assert_eq!(
            *asked.borrow(),
            vec![Sequence::new(5)],
            "the retry asked from somewhere other than where it failed"
        );
        assert_eq!(reached, Sequence::new(15));
    }

    #[test]
    fn a_collection_that_lands_advances_the_cursor() {
        let mut collecting = Collecting::from(Sequence::new(1));
        assert_eq!(
            collecting.once(|_| Ok::<Sequence, ()>(Sequence::new(9))),
            Sequence::new(9)
        );
        assert_eq!(collecting.reached(), Sequence::new(9));
    }

    #[test]
    fn a_renewal_that_wins_nothing_keeps_the_lease_it_holds() {
        let held = Leadership {
            epoch: Epoch::new(4),
            from: Instant::now(),
        };
        let mut renewing = Renewing::holding(held);
        let standing = renewing.once(|_, _| None);
        assert_eq!(standing, held, "a round that won nothing changed the lease");
        assert_eq!(renewing.standing(), held);
    }

    #[test]
    fn a_renewal_stands_for_the_epoch_after_the_one_it_holds() {
        let from = Instant::now();
        let mut renewing = Renewing::holding(Leadership {
            epoch: Epoch::new(4),
            from,
        });
        let stood_for = RefCell::new(Vec::new());
        let won = Leadership {
            epoch: Epoch::new(5),
            from,
        };
        let standing = renewing.once(|_: Lease, next| {
            stood_for.borrow_mut().push(next);
            Some(won)
        });
        assert_eq!(*stood_for.borrow(), vec![Epoch::new(5)]);
        assert_eq!(standing, won, "a round that was won was not taken up");
    }

    #[test]
    fn a_delayed_cadence_runs_once_however_many_periods_it_missed() {
        let period = Duration::from_secs(10);
        let ran_at = Instant::now();
        let late = ran_at
            .checked_add(Duration::from_secs(35))
            .expect("an instant 35s from now");
        assert_eq!(
            due_in(period, ran_at, late),
            Duration::ZERO,
            "a pass that overran by three periods asked for more than one catch-up"
        );
    }

    #[test]
    fn a_cadence_that_is_early_waits_out_the_remainder() {
        let period = Duration::from_secs(10);
        let ran_at = Instant::now();
        let soon = ran_at
            .checked_add(Duration::from_secs(3))
            .expect("an instant 3s from now");
        assert_eq!(due_in(period, ran_at, soon), Duration::from_secs(7));
    }

    #[test]
    fn a_cadence_runs_no_pass_once_the_node_is_asked_to_stop() {
        let stopping = Stopping::new();
        stopping.refuse_new();
        let passes = RefCell::new(0_usize);
        every(Duration::ZERO, &stopping, |_| {
            *passes.borrow_mut() += 1;
        });
        assert_eq!(
            *passes.borrow(),
            0,
            "a node already stopping still ran a cadence pass"
        );
    }

    #[test]
    fn a_greeting_round_carries_previous_readings_forward() {
        let published = Published::holding(Directory::new());
        let first = Instant::now();
        published.round(|directory| directory.heard("one:9080", said(), first));

        published.round(|directory| directory.heard("two:9080", said(), first));

        let current = published.current();
        assert!(
            current.age_of("one:9080", first).is_some(),
            "the peer heard in the first round vanished in the second"
        );
        assert!(current.age_of("two:9080", first).is_some());
    }

    #[test]
    fn a_greeting_round_publishes_nothing_until_it_is_done() {
        let published = Published::holding(Directory::new());
        let at = Instant::now();
        let during = RefCell::new(None);
        published.round(|directory| {
            directory.heard("one:9080", said(), at);
            *during.borrow_mut() = Some(Arc::clone(&published.current()));
        });
        let seen = during.borrow().clone().expect("the round ran");
        assert!(
            seen.age_of("one:9080", at).is_none(),
            "a reader saw a half-finished round"
        );
        assert!(published.current().age_of("one:9080", at).is_some());
    }
}
