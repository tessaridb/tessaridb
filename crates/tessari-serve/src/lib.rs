//! Stopping a serving process, in the order the stages have to happen.
//!
//! # Why this is a crate and not a module in one of the serving crates
//!
//! Both surfaces need the same four things — a way to say whether they are still
//! willing to take work, a way to be told to stop accepting, a count of what is
//! still in flight, and a way to say when that count has reached zero — and they
//! need them to mean the *same* thing, because
//! a shutdown that drains one surface and abandons the other is not a staged
//! shutdown. Putting this in the wire crate would make the HTTP crate depend on
//! it for no other reason; a copy in each gives two mechanisms that must agree
//! and will eventually not.
//!
//! Neither surface sequences the stages. That belongs to whatever holds both of
//! them, which is the binary — so this crate offers the pieces and the order
//! lives with the process (ADR-0015).
//!
//! # Requests and feeds are counted apart, and that is the whole design
//!
//! ADR-0015's stage 2 waits for in-flight work to finish and stage 3 then ends
//! subscriptions. A single count cannot tell those apart, and the difference is
//! exactly what makes them separate stages: **a request finishes on its own and
//! a subscription never does.** Counted together, stage 2 waits for a feed that
//! will still be there at the deadline, and every shutdown becomes a timeout.
//!
//! So a connection is counted as a request when it arrives and *moves* to the
//! feed count if it becomes a subscription — the same connection, a different
//! promise about whether waiting for it can succeed.
//!
//! # Not ready comes before not listening
//!
//! Refusing connections and being unwilling to serve are also two states rather
//! than one, for a reason with the same shape. A readiness route exists so a
//! load balancer can stop sending work *before* the port goes; if the same flag
//! did both, the port would close at the instant the answer changed and nothing
//! could ever observe it. So stage 0 sets [`Stopping::leaving`] and the process
//! keeps serving for a window, and stage 1 sets the refusal.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How long to wait between checks while draining.
///
/// Short enough that a shutdown of an idle node is not perceptibly delayed, long
/// enough that draining is not a spin. This parks the thread rather than
/// yielding: a drain waits far longer than a scheduler quantum, and yielding for
/// that long burns a core to no purpose.
const GLANCE: Duration = Duration::from_millis(10);

/// The state a stopping process shares with its surfaces.
///
/// Held behind an [`Arc`] by everything that reads or writes it: the surfaces
/// increment and decrement, the process asks and waits.
#[derive(Debug, Default)]
pub struct Stopping {
    leaving: AtomicBool,
    asked: AtomicBool,
    requests: AtomicUsize,
    feeds: AtomicUsize,
    answers: AtomicU64,
    refusals: AtomicU64,
}

impl Stopping {
    /// A process that has not been asked to stop.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Stage 0 — say *not ready* while still serving.
    ///
    /// Separate from [`Stopping::refuse_new`] because the two are read by
    /// different things for different reasons: a readiness route reads
    /// *willingness*, and an accept loop reads *permission*. Collapsing them
    /// into one flag closes the port at the moment the answer changes, which
    /// leaves nothing able to connect and ask — and a readiness route nobody
    /// can reach when it matters is decoration.
    pub fn leaving(&self) {
        self.leaving.store(true, Ordering::Release);
    }

    /// Whether this node still takes new work.
    ///
    /// False from stage 0 onwards. Monotone: nothing here becomes ready again,
    /// because a process on its way out has no state to come back to.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.leaving.load(Ordering::Acquire)
    }

    /// Stage 1 — refuse new connections.
    ///
    /// Only sets the intent. Waking a listener that is blocked in `accept` is
    /// the surface's own problem, because how you interrupt it depends on what
    /// it is: an HTTP server here has a call for it, and a plain TCP listener is
    /// woken by connecting to it.
    ///
    /// Implies stage 0, so a caller that skips the lame-duck window still leaves
    /// a coherent state behind rather than a node that refuses connections while
    /// telling anything that can still reach it that it is ready.
    pub fn refuse_new(&self) {
        self.leaving.store(true, Ordering::Release);
        self.asked.store(true, Ordering::Release);
    }

    /// Whether stopping has been asked for.
    ///
    /// Checked by an accept loop before it takes the next connection, so a
    /// connection accepted in the race is served rather than dropped — which is
    /// the right way round: refusing to *accept* is the promise, not refusing to
    /// finish.
    #[must_use]
    pub fn asked(&self) -> bool {
        self.asked.load(Ordering::Acquire)
    }

    /// Count one connection as in flight until the returned guard is dropped.
    #[must_use]
    pub fn busy(self: &Arc<Self>) -> Busy {
        self.requests.fetch_add(1, Ordering::AcqRel);
        Busy {
            held: Arc::clone(self),
            feed: false,
        }
    }

    /// Requests still in flight.
    #[must_use]
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }

    /// Subscriptions still open.
    #[must_use]
    pub fn feeds(&self) -> usize {
        self.feeds.load(Ordering::Acquire)
    }

    /// Record one answer, and whether it was a refusal.
    ///
    /// A **refusal** is a request the node answered with a failure instead of a
    /// result. That is one definition and each surface maps its own vocabulary
    /// onto it — an HTTP status of 400 or more, a wire frame of the refusal
    /// kind — so the mapping lives in one place per surface rather than at every
    /// site that writes an answer.
    ///
    /// Deliberately *not* "connections turned away while stopping". That number
    /// is also interesting and is a different one: an accept loop that has been
    /// told to stop simply stops, so there is no per-refusal event to count, and
    /// building one to feed a metric would be the metric wagging the mechanism.
    pub fn answered(&self, refused: bool) {
        self.answers.fetch_add(1, Ordering::AcqRel);
        if refused {
            self.refusals.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Answers written since this surface started, refusals included.
    #[must_use]
    pub fn answers(&self) -> u64 {
        self.answers.load(Ordering::Acquire)
    }

    /// How many of those answers were refusals.
    #[must_use]
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Acquire)
    }

    /// Stage 2 — wait for in-flight requests, and say whether they finished.
    ///
    /// Waits on requests **only**. Feeds are stage 3 and waiting for them here
    /// is the mistake this type exists to make impossible.
    #[must_use]
    pub fn drain(&self, patience: Duration) -> Drained {
        let began = Instant::now();
        while self.requests() > 0 {
            if began.elapsed() >= patience {
                return Drained::Deadline {
                    left: self.requests(),
                };
            }
            std::thread::park_timeout(GLANCE);
        }
        Drained::Finished
    }
}

/// Every surface this process is serving, and when the process started.
///
/// # Why one surface cannot report on its own
///
/// A metrics route lives on **one** surface but must describe the **process**:
/// "open connections per surface" includes the wire protocol, whose counters an
/// HTTP node has never seen. Something has to hold both, and the process already
/// does — it enumerates every surface to sequence the shutdown. This is that
/// same list, shared rather than rebuilt, so the numbers a scrape reports and
/// the numbers a drain waits on cannot drift apart.
///
/// The process start is here rather than beside it because it is the same kind
/// of fact: true of the process, not of any one listener.
#[derive(Debug)]
pub struct Census {
    started: Instant,
    surfaces: Vec<(&'static str, Arc<Stopping>)>,
}

impl Census {
    /// A census of a process that started at `started`.
    ///
    /// Taken from the caller rather than read here, because the interesting
    /// moment is when the **process** began and this is built later — after the
    /// store is open, which is exactly the interval a restart-detector cares
    /// about and would be silently excluded.
    #[must_use]
    pub const fn since(started: Instant) -> Self {
        Self {
            started,
            surfaces: Vec::new(),
        }
    }

    /// Count `name` among this process's surfaces.
    pub fn counting(&mut self, name: &'static str, stopping: Arc<Stopping>) {
        self.surfaces.push((name, stopping));
    }

    /// How long the process has been running.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Each surface, by name.
    pub fn surfaces(&self) -> impl Iterator<Item = (&'static str, &Stopping)> {
        self.surfaces
            .iter()
            .map(|(name, stopping)| (*name, stopping.as_ref()))
    }
}

/// How a drain ended.
///
/// Two outcomes rather than a `bool`, because "the deadline passed with four
/// requests still running" is what an operator needs to see in a log, and a
/// `false` would have thrown the number away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// Everything in flight finished.
    Finished,
    /// The deadline passed first, with this many still running.
    Deadline {
        /// How many requests were still in flight.
        left: usize,
    },
}

/// One connection, counted for as long as this is alive.
///
/// The count is decremented on **drop** rather than at the end of the work,
/// deliberately: a connection thread that panics must not leave the count
/// permanently high, because then the drain never reaches zero and every
/// shutdown from then on hits its deadline instead — a failure that appears
/// long after the panic that caused it.
#[derive(Debug)]
pub struct Busy {
    held: Arc<Stopping>,
    feed: bool,
}

impl Busy {
    /// This connection has become a subscription.
    ///
    /// Moves it from the request count to the feed count. It is a move and not
    /// a second increment because it is still one connection — counting it
    /// twice would leave stage 2 waiting for something stage 3 owns.
    pub fn became_a_feed(&mut self) {
        if !self.feed {
            self.held.requests.fetch_sub(1, Ordering::AcqRel);
            self.held.feeds.fetch_add(1, Ordering::AcqRel);
            self.feed = true;
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if self.feed {
            self.held.feeds.fetch_sub(1, Ordering::AcqRel);
        } else {
            self.held.requests.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// How many connections a surface will serve at once.
///
/// # Why this is here rather than in each surface
///
/// The same argument the rest of this crate is built on: both surfaces need a
/// ceiling, and they need it to mean the same thing. A process told to serve at
/// most four hundred connections, whose wire and HTTP halves each counted to
/// four hundred separately, has been told nothing.
///
/// # Why a refusal and not a queue
///
/// Because the ceiling exists to bound a resource, and a queue does not bound
/// one — it moves the unbounded growth from threads to whatever holds the
/// waiting connections, and adds latency to the connections that were admitted.
/// A client refused at the door can retry, reconnect elsewhere, or back off; a
/// client parked in a queue can only wait, and cannot tell that it is waiting.
#[derive(Debug)]
pub struct Admitting {
    held: AtomicUsize,
    limit: usize,
    refused: AtomicU64,
}

impl Admitting {
    /// A door that will hold `limit` connections open at once.
    #[must_use]
    pub fn to(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            held: AtomicUsize::new(0),
            limit,
            refused: AtomicU64::new(0),
        })
    }

    /// Take a place, or `None` when the ceiling is reached.
    ///
    /// Never blocks. See the type's documentation for why waiting here would
    /// give back the exhaustion the ceiling exists to prevent.
    pub fn admit(self: &Arc<Self>) -> Option<Admitted> {
        let mut held = self.held.load(Ordering::Acquire);
        loop {
            if held >= self.limit {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            match self.held.compare_exchange_weak(
                held,
                held.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Admitted {
                        door: Arc::clone(self),
                    });
                }
                // Another thread moved the count between the read and the
                // exchange. Re-decide against what it actually is rather than
                // against what it was, which is the whole point of the loop.
                Err(actual) => held = actual,
            }
        }
    }

    /// How many places are taken.
    #[must_use]
    pub fn open(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// The ceiling this door was built with.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// How many connections have been turned away since the process started.
    ///
    /// The number that says whether the ceiling is set right: a node refusing
    /// steadily is a node whose ceiling is too low or whose clients are too
    /// many, and one refusing never has a ceiling it has never reached.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
}

/// One admitted connection, holding its place until dropped.
///
/// Released on **drop** for the same reason [`Busy`] is: a connection thread
/// that panics must not take a place with it, or the door closes permanently
/// one connection at a time and the failure appears long after its cause.
#[derive(Debug)]
pub struct Admitted {
    door: Arc<Admitting>,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.door.held.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    // The panic below is the subject of a test, not a hazard in one: a place
    // must come back when the thread holding it dies.
    #![allow(clippy::panic, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_door_admits_up_to_its_limit_and_then_refuses() {
        let door = Admitting::to(2);
        let first = door.admit().expect("the first is under the limit");
        let second = door.admit().expect("the second reaches it");
        assert_eq!(door.open(), 2);
        assert!(door.admit().is_none(), "the third is over it");
        assert_eq!(door.refused(), 1);

        // Refusing must not be mistaken for a leak: the two that were admitted
        // are still held, and the count still says two.
        assert_eq!(door.open(), 2);
        drop(first);
        assert_eq!(door.open(), 1);
        assert!(door.admit().is_some(), "a place freed is a place given");
        drop(second);
    }

    #[test]
    fn a_place_is_returned_even_when_the_thread_holding_it_panics() {
        // The property the `Drop` impl exists for. Without it a panicking
        // connection thread closes the door by one, permanently, and the node
        // degrades in a way nothing points at the panic that caused it.
        let door = Admitting::to(1);
        let held = Arc::clone(&door);
        let panicked = std::thread::spawn(move || {
            let _place = held.admit().expect("the only place");
            panic!("a connection that went wrong");
        })
        .join();
        assert!(panicked.is_err(), "the thread should have panicked");
        assert_eq!(door.open(), 0, "the place should have come back");
        assert!(door.admit().is_some());
    }

    #[test]
    fn every_thread_racing_for_the_last_places_sees_one_ceiling() {
        // The compare-exchange loop's reason to exist. A read-then-add would let
        // two threads both see `limit - 1` and both admit, which is the bug an
        // atomic counter without a loop actually has.
        let door = Admitting::to(50);
        let taken: Vec<_> = (0..8)
            .map(|_| {
                let held = Arc::clone(&door);
                std::thread::spawn(move || (0..20).filter_map(|_| held.admit()).collect::<Vec<_>>())
            })
            .map(|racing| racing.join().expect("a racing thread"))
            .collect();
        let admitted: usize = taken.iter().map(Vec::len).sum();
        assert_eq!(admitted, 50, "never more than the ceiling, and never fewer");
        assert_eq!(door.open(), 50);
        assert_eq!(door.refused(), 110);
    }

    #[test]
    fn a_guard_that_is_dropped_is_no_longer_in_flight() {
        let stopping = Stopping::new();
        assert_eq!(stopping.requests(), 0);
        let one = stopping.busy();
        let two = stopping.busy();
        assert_eq!(stopping.requests(), 2);
        drop(one);
        assert_eq!(stopping.requests(), 1);
        drop(two);
        assert_eq!(stopping.requests(), 0);
    }

    #[test]
    fn a_connection_that_becomes_a_feed_leaves_the_request_count() {
        // The property stage 2 depends on: once a connection is a feed, waiting
        // for requests can still succeed. Counted together it never could.
        let stopping = Stopping::new();
        let mut held = stopping.busy();
        assert_eq!((stopping.requests(), stopping.feeds()), (1, 0));
        held.became_a_feed();
        assert_eq!((stopping.requests(), stopping.feeds()), (0, 1));
        // Idempotent, because a caller that says it twice has not opened a
        // second subscription.
        held.became_a_feed();
        assert_eq!((stopping.requests(), stopping.feeds()), (0, 1));
        drop(held);
        assert_eq!((stopping.requests(), stopping.feeds()), (0, 0));
    }

    #[test]
    fn a_drain_waits_for_a_request_and_not_for_a_feed() {
        let stopping = Stopping::new();
        let mut feed = stopping.busy();
        feed.became_a_feed();
        // A feed is open, and the drain still finishes — which is the whole
        // reason the two counts are apart. Waiting on both, this would time out.
        assert_eq!(
            stopping.drain(Duration::from_secs(5)),
            Drained::Finished,
            "the drain waited for a subscription, which never ends on its own"
        );

        let working = stopping.busy();
        let patience = Duration::from_millis(30);
        assert_eq!(
            stopping.drain(patience),
            Drained::Deadline { left: 1 },
            "the drain did not wait for a request that never finished"
        );
        drop(working);
        assert_eq!(stopping.drain(patience), Drained::Finished);
    }

    #[test]
    fn a_node_stops_being_ready_before_it_stops_accepting() {
        // The window the readiness route is reached in. If these were one flag
        // the port would close at the instant the answer changed, and nothing
        // outside the process could ever see the 503.
        let stopping = Stopping::new();
        assert!(stopping.ready());
        assert!(!stopping.asked());

        stopping.leaving();
        assert!(
            !stopping.ready(),
            "a leaving node still called itself ready"
        );
        assert!(
            !stopping.asked(),
            "stage 0 closed the port, so the readiness answer it just changed \
             cannot be reached by anything"
        );

        stopping.refuse_new();
        assert!(!stopping.ready());
        assert!(stopping.asked());
    }

    #[test]
    fn refusing_connections_implies_no_longer_being_ready() {
        // A caller that skips stage 0 must not leave a node refusing new
        // connections while telling whatever can still reach it that it is
        // ready to take them.
        let stopping = Stopping::new();
        stopping.refuse_new();
        assert!(!stopping.ready());
    }

    #[test]
    fn refusals_are_counted_among_answers_and_not_beside_them() {
        // `answers` includes refusals, so a dashboard can show a rate of one
        // against the other without a third number to keep consistent. Counted
        // beside each other instead, "how many requests did this node answer"
        // would need an addition that somebody eventually gets wrong.
        let stopping = Stopping::new();
        assert_eq!((stopping.answers(), stopping.refusals()), (0, 0));
        stopping.answered(false);
        stopping.answered(true);
        stopping.answered(false);
        assert_eq!(
            (stopping.answers(), stopping.refusals()),
            (3, 1),
            "a refusal was not counted as an answer"
        );
    }

    #[test]
    fn a_census_reports_every_surface_and_the_process_that_holds_them() {
        // The property a metrics route depends on and one surface cannot have:
        // it describes the process, so it must see counters it did not create.
        let wire = Stopping::new();
        let http = Stopping::new();
        wire.answered(false);

        let mut census = Census::since(Instant::now());
        census.counting("wire", Arc::clone(&wire));
        census.counting("http", Arc::clone(&http));

        let seen: Vec<_> = census
            .surfaces()
            .map(|(name, counted)| (name, counted.answers()))
            .collect();
        assert_eq!(seen, vec![("wire", 1), ("http", 0)]);
    }

    #[test]
    fn stopping_is_asked_for_once_and_stays_asked() {
        let stopping = Stopping::new();
        assert!(!stopping.asked());
        stopping.refuse_new();
        assert!(stopping.asked());
        stopping.refuse_new();
        assert!(stopping.asked());
    }
}
