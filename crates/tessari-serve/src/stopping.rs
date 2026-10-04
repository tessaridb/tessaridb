//! The stages a serving process stops in, and the work it waits for.

use super::*;

/// How long to wait between checks while draining.
///
/// Short enough that a shutdown of an idle node is not perceptibly delayed, long
/// enough that draining is not a spin. The drain sleeps on the runtime's timer
/// rather than yielding: it waits far longer than a scheduler quantum, and
/// yielding for that long burns a core to no purpose.
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
    redirects_settled: AtomicU64,
    redirects_transient: AtomicU64,
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
        self.answers.fetch_add(1, Ordering::Relaxed);
        if refused {
            self.refusals.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Answers written since this surface started, refusals included.
    #[must_use]
    pub fn answers(&self) -> u64 {
        self.answers.load(Ordering::Relaxed)
    }

    /// How many of those answers were refusals.
    #[must_use]
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    /// Record one answer that sent the caller to another node (G053 C6):
    /// `settled` when it names a leadership the caller may remember for the
    /// range, transient when it names a node for this read alone. Counted by
    /// each surface where it decides the redirect, beside [`Self::answered`].
    pub fn redirected(&self, settled: bool) {
        let counter = if settled {
            &self.redirects_settled
        } else {
            &self.redirects_transient
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Redirects sent since this surface started, as `(settled, transient)`.
    #[must_use]
    pub fn redirects(&self) -> (u64, u64) {
        (
            self.redirects_settled.load(Ordering::Relaxed),
            self.redirects_transient.load(Ordering::Relaxed),
        )
    }

    /// Stage 2 — wait for in-flight requests, and say whether they finished.
    ///
    /// Waits on requests **only**. Feeds are stage 3 and waiting for them here
    /// is the mistake this type exists to make impossible.
    ///
    /// Must be awaited inside a Tokio runtime with its timer enabled.
    pub async fn drain(&self, patience: Duration) -> Drained {
        let began = tokio::time::Instant::now();
        while self.requests() > 0 {
            if began.elapsed() >= patience {
                return Drained::Deadline {
                    left: self.requests(),
                };
            }
            tokio::time::sleep(GLANCE).await;
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
