//! The load-balancing policy the store line's leader runs a placement move
//! under, beside the failover policy.
//!
//! # Why a row rather than a flag
//!
//! For [`crate::failover`]'s reason, one level down: the margins here decide
//! when serving work moves between nodes, and every node has to agree about
//! when that may happen, because two nodes applying different load policies
//! are two nodes each moving the same placement toward a different home — the
//! flapping the spacing clause exists to prevent, arriving through the
//! mechanism meant to prevent it. An unreplicated flag cannot carry that
//! agreement, so the policy is a catalog row, ordered by the same `(epoch,
//! version)` pair the failover row carries.
//!
//! # The ratio is hundredths, not a float
//!
//! `ABOVE 1.5` arrives as hundredths (150) before it reaches this crate: the
//! statement tree keeps an `Eq`-preserving fixed-point spelling for the same
//! reason this one does — a float field would take total equality away from
//! every comparison over the policy. Hundredths lose nothing a ratio needs:
//! a margin is a small count of one, not a measurement.

use std::time::Duration;

use tessari_constants::BALANCE_SECONDS;

use crate::error::{Error, Result};
use crate::failover::Failover;

/// A load-balancing policy as an operator stated it.
///
/// A [`Balancer`] in hand is one whose relations hold, because
/// [`Balancer::stated`] is the only way to build one — so a row read back is
/// re-checked at the catalog's `from_value` rather than trusted. A stored row
/// is bytes on a disk, and bytes can be older than the relations, or edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balancer {
    /// The load margin a move waits for, as hundredths of one (150 is 1.5):
    /// the busiest measured range's rate must exceed the quietest eligible
    /// voter's by this factor before the pass moves anything.
    above_hundredths: u64,
    /// The window the rates are measured over.
    within: Duration,
    /// How little time must pass before a range the pass moved may be
    /// considered again — the anti-flapping bound.
    spacing: Duration,
}

impl Balancer {
    /// A policy as an operator stated it, refused if its clauses do not hold
    /// together with the failover policy it runs beside.
    ///
    /// # Errors
    ///
    /// [`Error::BalancerMarginNotAboveOne`] when the margin is at or below
    /// one — a margin of one would move a placement on any noise. (The
    /// statement refuses this where the number is too; the check here is the
    /// one a read-back row earns, because stored bytes are not evidence.)
    ///
    /// [`Error::BalancerSpacingBelowLease`] when the spacing is shorter than
    /// one lease period of the failover policy — the **too small** direction.
    /// A move re-examined while the election it provoked may still be settling
    /// is how a range flaps: the two bounds are one bound, and it lives on the
    /// longer of them.
    ///
    /// [`Error::BalancerWithinBelowCadence`] when the measured window is
    /// shorter than the round's cadence — the **too small** direction again. A
    /// window briefer than the pass that fills it measures nothing: the pass
    /// reads the window the log holds, and the log holds one pass's worth at
    /// most.
    pub fn stated(
        above_hundredths: u64,
        within: Duration,
        spacing: Duration,
        failover: &Failover,
    ) -> Result<Self> {
        let stated = Self {
            above_hundredths,
            within,
            spacing,
        };
        stated.holds_together(failover)?;
        Ok(stated)
    }

    /// The margin a move waits for, as hundredths of one.
    #[must_use]
    pub const fn above_hundredths(&self) -> u64 {
        self.above_hundredths
    }

    /// The window the rates are measured over.
    #[must_use]
    pub const fn within(&self) -> Duration {
        self.within
    }

    /// How soon a range the pass moved may be considered again.
    #[must_use]
    pub const fn spacing(&self) -> Duration {
        self.spacing
    }

    /// The relations the clauses hold one another to, against the failover
    /// policy the pass runs beside.
    fn holds_together(&self, failover: &Failover) -> Result<()> {
        if self.above_hundredths <= 100 {
            return Err(Error::BalancerMarginNotAboveOne {
                stated: self.above_hundredths,
            });
        }
        if self.spacing < failover.lease() {
            return Err(Error::BalancerSpacingBelowLease {
                spacing: self.spacing,
                lease: failover.lease(),
            });
        }
        let cadence = Duration::from_secs(BALANCE_SECONDS);
        if self.within < cadence {
            return Err(Error::BalancerWithinBelowCadence {
                within: self.within,
                cadence,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    /// A policy inside every relation, against the failover policy the build
    /// ships — the default's lease is the bound the spacing clause is judged
    /// against here.
    fn stated() -> Balancer {
        Balancer::stated(
            150,
            Duration::from_secs(30),
            Failover::DEFAULT.lease(),
            &Failover::DEFAULT,
        )
        .expect("a policy inside every relation is accepted")
    }

    #[test]
    fn a_stated_policy_reads_back_exactly_what_was_stated() {
        let policy = stated();
        assert_eq!(policy.above_hundredths(), 150);
        assert_eq!(policy.within(), Duration::from_secs(30));
        assert_eq!(policy.spacing(), Failover::DEFAULT.lease());
    }

    #[test]
    fn a_margin_of_one_is_refused_and_named() {
        let refused = Balancer::stated(
            100,
            Duration::from_secs(30),
            Failover::DEFAULT.lease(),
            &Failover::DEFAULT,
        )
        .expect_err("a margin of one is not a margin");
        let said = refused.to_string();
        assert!(
            said.contains("not above 1"),
            "the refusal did not name the direction: {said}"
        );
    }

    #[test]
    fn a_spacing_below_one_lease_is_refused_and_names_both() {
        let below = Failover::DEFAULT
            .lease()
            .checked_sub(Duration::from_millis(100))
            .expect("the shipped lease exceeds a hundred milliseconds");
        let refused = Balancer::stated(150, Duration::from_secs(30), below, &Failover::DEFAULT)
            .expect_err("a spacing below one lease period is accepted");
        let said = refused.to_string();
        assert!(
            said.contains("SPACING") && said.contains("LEASE"),
            "the refusal did not name the clause and the bound: {said}"
        );
    }

    #[test]
    fn a_window_below_the_rounds_cadence_is_refused_and_named() {
        let refused = Balancer::stated(
            150,
            Duration::from_secs(BALANCE_SECONDS - 1),
            Failover::DEFAULT.lease(),
            &Failover::DEFAULT,
        )
        .expect_err("a window briefer than the round's cadence is accepted");
        let said = refused.to_string();
        assert!(
            said.contains("WITHIN") && said.contains("cadence"),
            "the refusal did not name the clause and the bound: {said}"
        );
    }
}
