//! Leases and the lines they lead.

use tessari_types::Epoch;

use crate::catalog::Reach;

use super::Store;

impl Store {
    /// Take or renew the lease this process writes under.
    ///
    /// The fence closes `LEASE_GUARD` before the lease expires, so this node
    /// stops writing strictly before the cluster is entitled to give the
    /// leadership to somebody else. See [`crate::lease`] for why the two
    /// instants are deliberately not the same one.
    ///
    /// Nothing in this build calls it but a test: granting a lease is a cluster
    /// act and needs a wire. What exists here is the fence.
    pub fn hold_lease(&self, ttl: std::time::Duration) {
        self.lease.take(ttl);
    }

    /// Hold a lease a majority granted, exactly as it was granted.
    ///
    /// The seam between the cluster and the engine, and the reason it takes a
    /// whole [`Lease`] rather than a span: a granted lease is dated from the
    /// instant its round **opened**, and a duration arriving here cannot carry
    /// that instant — it would restart the clock at the moment of installation,
    /// so every millisecond the round spent collecting would come out of the
    /// **voters'** window instead of this node's. That is the split-brain the
    /// dating rule exists to prevent, reached through the seam rather than
    /// through the rule.
    ///
    /// Nothing is re-checked here. Whether the grant was legitimate was settled
    /// by the round; a store asking again would be asking about a fact it has no
    /// way to know.
    pub fn hold(&self, epoch: Epoch, lease: crate::lease::Lease) {
        self.lease.hold(lease);
        if let Ok(mut leading) = self.leading.lock() {
            *leading = Some(epoch);
        }
    }

    /// The leadership epoch this node is writing under, if a round granted it
    /// one.
    ///
    /// A greeting says *the leadership it believes is current*, and until this
    /// existed the only honest answer a serving node could give was the constant
    /// zero — true while nothing campaigned, and a lie to every peer the moment
    /// something did.
    ///
    /// `None` is not zero. A node nobody elected is not leading under the first
    /// epoch; it is not leading at all, and a caller that wants the constant for
    /// a store that never campaigns can say so in one word at its own call site.
    #[must_use]
    pub fn leading(&self) -> Option<Epoch> {
        match self.leading.lock() {
            Ok(leading) => *leading,
            // The fence's decision, for the fence's reason: a diagnostic that
            // fails open leaves a gap in a report, and this one feeds a greeting
            // a peer routes on. Saying nothing is the conservative answer, and
            // it is the one a node that never campaigned gives anyway.
            Err(_) => None,
        }
    }

    /// Hold a lease a majority granted on `range`'s own line (ADR-0082).
    ///
    /// The store line is [`Self::hold`]; this is every other line, and the two
    /// are one call so a caller campaigning for several ranges does not choose
    /// the seam by hand.
    pub fn hold_range(&self, range: Reach, epoch: Epoch, lease: crate::lease::Lease) {
        if range == Reach::Store {
            self.hold(epoch, lease);
        } else {
            self.lines.hold(range, epoch, lease);
        }
    }

    /// The epoch this node holds on `range`'s own line, if a round granted it
    /// one — [`Self::leading`] for the store line.
    ///
    /// A placed range's line answers only while its lease is live; the store
    /// line's epoch keeps its old meaning and outlives the lease.
    #[must_use]
    pub fn leading_of(&self, range: Reach) -> Option<Epoch> {
        if range == Reach::Store {
            self.leading()
        } else {
            self.lines.epoch_of(range)
        }
    }

    /// Where this node stands on a placed range's line.
    pub(crate) fn line_standing(&self, range: Reach) -> crate::lines::Standing {
        self.lines.standing(range)
    }

    /// Whether this node holds any placed range's line.
    pub(crate) fn holds_lines(&self) -> bool {
        self.lines.any()
    }

    /// How long this node's lease fence has been closed, if it is.
    ///
    /// `None` means writes may proceed — either because the fence is still open
    /// or because this node was never given a lease at all. A node nobody
    /// granted leadership to is not a leader running out of it.
    #[must_use]
    pub fn lease_spent(&self) -> Option<std::time::Duration> {
        self.lease.spent()
    }
}
