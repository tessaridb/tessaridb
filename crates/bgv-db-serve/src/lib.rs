//! Stopping a serving process, in the order the stages have to happen.
//!
//! # Why this is a crate and not a module in one of the serving crates
//!
//! Both surfaces need the same three things — a way to be told to stop
//! accepting, a count of what is still in flight, and a way to say when that
//! count has reached zero — and they need them to mean the *same* thing, because
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

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    asked: AtomicBool,
    requests: AtomicUsize,
    feeds: AtomicUsize,
}

impl Stopping {
    /// A process that has not been asked to stop.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Stage 1 — refuse new connections.
    ///
    /// Only sets the intent. Waking a listener that is blocked in `accept` is
    /// the surface's own problem, because how you interrupt it depends on what
    /// it is: an HTTP server here has a call for it, and a plain TCP listener is
    /// woken by connecting to it.
    pub fn refuse_new(&self) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn stopping_is_asked_for_once_and_stays_asked() {
        let stopping = Stopping::new();
        assert!(!stopping.asked());
        stopping.refuse_new();
        assert!(stopping.asked());
        stopping.refuse_new();
        assert!(stopping.asked());
    }
}
