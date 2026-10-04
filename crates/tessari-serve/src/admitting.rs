//! The door: how many connections a surface holds, and the place each one takes.

use super::*;

/// How many of something a process will hold at once.
///
/// Two callers so far, and they are the same shape: how many connections a
/// surface will serve, and how many password verifications the process will run
/// — each holding a resource for as long as it lives, each better refused than
/// queued.
///
/// # Why this is here rather than in each caller
///
/// The same argument the rest of this crate is built on: the callers need a
/// ceiling, and they need it to mean the same thing. A process told to serve at
/// most four hundred connections, whose wire and HTTP halves each counted to
/// four hundred separately, has been told nothing — and a process told to hold
/// a hundred and fifty mebibytes of password hashing, counted separately per
/// surface, has been told less than nothing.
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
    /// A door that will hold `limit` places at once.
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
