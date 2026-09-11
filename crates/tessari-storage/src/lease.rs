//! The lease a leader writes under, and the fence it closes when it runs out.
//!
//! # What a lease is for
//!
//! A leader that has lost the cluster does not know it has. It keeps accepting
//! writes, and every one of them is a write the next leader will not have. The
//! lease is the answer: leadership is held for a bounded time, it has to be
//! renewed to continue, and a leader that has not renewed **stops writing on its
//! own** without needing to be told.
//!
//! # Two clocks, and this one takes the local one
//!
//! A lease is a fence, so it runs on **monotonic elapsed time** —
//! [`std::time::Instant`], never a wall clock and never the logged instant the
//! rest of the engine writes into records. The reason is one failure mode: an
//! NTP step backwards extends a lease that should already have died, and a fence
//! that can be extended by a clock adjustment is not a fence. Nothing in this
//! module reads `time::now()`.
//!
//! # The fence closes before the grant opens
//!
//! The half usually left out. The holder stops accepting writes at **T**; the
//! cluster considers the lease dead at **T + δ**. They are deliberately not the
//! same instant, and δ covers the difference in rate between two clocks that
//! were never synchronised to each other. If the two coincided, a holder whose
//! clock ran a little slow would still be writing at the moment somebody else
//! was granted the same leadership — which is the split-brain the lease exists
//! to prevent, arriving through the mechanism meant to prevent it.
//!
//! A TTL at or below δ leaves no room at all, and is therefore fenced from the
//! instant it is taken. That is the safe direction: a lease too short to be
//! useful refuses writes rather than granting a window nobody can bound.
//!
//! # What is not here
//!
//! **Nothing grants a lease.** Granting is a cluster act and needs the wire that
//! does not exist yet. This module is the fence, and a store that has never been
//! given a lease is not fenced by it — a node nobody granted leadership to is
//! not a leader running out of it. Once granting exists, *writing without a
//! lease* becomes the thing to refuse, and that is a different wave.
//!
//! Nothing here is persisted either, and the reason is [`crate::store`]'s own:
//! an unreplicated file asserting a cluster-wide fact **is** the split-brain. A
//! persisted fence is that same claim with a deadline attached.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How far the holder's fence closes ahead of the grantor's expiry.
///
/// Covers the rate difference between two clocks nobody synchronised. Not
/// configurable in this build, because a value an operator can lower to zero is
/// a value somebody will lower to zero the day a lease refuses a write they
/// wanted.
pub const GUARD: Duration = Duration::from_secs(2);

/// Leadership held for a bounded time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    /// When this holder stops writing.
    fence: Instant,
    /// When the cluster is entitled to consider it dead.
    expiry: Instant,
}

impl Lease {
    /// Take a lease for `ttl`, starting now.
    #[must_use]
    pub fn taken(ttl: Duration) -> Self {
        Self::taken_at(Instant::now(), ttl)
    }

    /// Take a lease at a stated instant, so the arithmetic can be tested
    /// without waiting for a clock.
    #[must_use]
    pub fn taken_at(taken_at: Instant, ttl: Duration) -> Self {
        let expiry = taken_at.checked_add(ttl).unwrap_or(taken_at);
        // Floored at the moment it was taken rather than allowed to go
        // backwards: a TTL shorter than the guard buys no writable window, and
        // the honest answer to that is a lease that is spent on arrival.
        let fence = expiry.checked_sub(GUARD).unwrap_or(taken_at).max(taken_at);
        Self { fence, expiry }
    }

    /// When this holder stops writing.
    #[must_use]
    pub const fn fence(&self) -> Instant {
        self.fence
    }

    /// When the cluster may consider this lease dead.
    #[must_use]
    pub const fn expiry(&self) -> Instant {
        self.expiry
    }

    /// Whether the fence has closed as of `now`.
    #[must_use]
    pub fn fenced(&self, now: Instant) -> bool {
        now >= self.fence
    }

    /// How long the fence has been closed as of `now`, or zero.
    #[must_use]
    pub fn spent_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.fence)
    }

    /// How much writable time is left as of `now`, or zero.
    ///
    /// Measured to the **fence** and not to the expiry, because the fence is
    /// what actually stops this node writing. The δ between them belongs to the
    /// cluster, not to the holder, and a holder shown the larger number would be
    /// shown a window it may not use.
    #[must_use]
    pub fn left(&self, now: Instant) -> Duration {
        self.fence.saturating_duration_since(now)
    }
}

/// The lease this process is writing under, if it was given one.
///
/// Shared by every handle to one store, because two handles are not two leaders
/// and a fence half the writers cannot see is not a fence.
#[derive(Debug, Default)]
pub struct Held {
    lease: Mutex<Option<Lease>>,
}

impl Held {
    /// Take or renew the lease.
    pub fn take(&self, ttl: Duration) {
        if let Ok(mut held) = self.lease.lock() {
            *held = Some(Lease::taken(ttl));
        }
    }

    /// Whether writes are fenced, and for how long they have been.
    ///
    /// # A poisoned lock refuses
    ///
    /// The opposite decision from [`crate::followers`], which loses a row
    /// rather than fail a follower's read — and the asymmetry is the point. A
    /// diagnostic that fails open leaves a gap in a report; a fence that fails
    /// open is not a fence, and the one moment it would fail open is a moment
    /// something has already gone wrong.
    ///
    /// Reported as the duration the fence has been closed rather than as a bare
    /// boolean, because a refusal that will not say whether the caller is one
    /// second or one hour past it leaves an operator guessing.
    pub fn spent(&self) -> Option<Duration> {
        match self.lease.lock() {
            Ok(held) => {
                let lease = (*held)?;
                let now = Instant::now();
                lease.fenced(now).then(|| lease.spent_for(now))
            }
            Err(_) => Some(Duration::ZERO),
        }
    }

    /// How long this node may still write, or `None` if it was never made a
    /// leader.
    ///
    /// The counterpart of [`Held::spent`] and the one an operator watches: a
    /// signal that appears only once the window has shut is a post-mortem, while
    /// a number that is normally positive and heads toward zero is an alarm. The
    /// split-brain window is exactly *this reached zero and the node is still
    /// writing*.
    ///
    /// `None` is a different statement from `Some(0)`. A node nobody granted
    /// leadership to is not a leader running out of time, and reporting it as
    /// one would put every single-node store permanently at zero.
    ///
    /// # A poisoned lock reports zero, not absence
    ///
    /// Matching [`Held::spent`] rather than [`crate::followers`]. The two are
    /// one question asked twice, so they must never disagree: a diagnostic
    /// saying *you have time left* while the fence has already closed is worse
    /// than no diagnostic. The invariant is that this reads `Some(0)` in exactly
    /// the cases `spent` reads `Some`.
    pub fn remaining(&self) -> Option<Duration> {
        match self.lease.lock() {
            Ok(held) => {
                let lease = (*held)?;
                Some(lease.left(Instant::now()))
            }
            Err(_) => Some(Duration::ZERO),
        }
    }
}
