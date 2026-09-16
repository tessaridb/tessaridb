//! How long a cluster waits before it decides a leader is gone.
//!
//! # Why this is a value and not five constants
//!
//! Every period here is a constant today, and every one of them is right for the
//! network this build was measured on. The moment a cluster spans two sites, a
//! second of canvass is optimistic and ten seconds of awareness is slow — and
//! neither number is reachable without a rebuild. A policy is the same numbers
//! with an operator at the other end of them.
//!
//! # The reason a policy needs refusals and a constant did not
//!
//! Three of these values are **derived from one another** in the constants they
//! default to: the campaign cadence from the round time, the staleness floor and
//! the collection period from the awareness interval. Today those relations hold
//! because the *compiler* holds them — nobody can write a campaign cadence
//! slower than the window it has to fit inside, because nobody writes it at all.
//!
//! Hand the numbers to an operator and the relations become breakable, and each
//! one breaks **silently**. A campaign cadence slower than the window between
//! *time to stand* and *the fence shuts* does not fail: the leader simply steps
//! over the moment it was supposed to renew, loses a lease it could have kept,
//! and nothing anywhere reports an error because no round was ever attempted. A
//! collection period at or above the staleness floor does not fail either — the
//! node goes on advertising a bound it cannot meet while everything is working.
//!
//! So a policy that merely *stores* five numbers has moved the failure rather
//! than enabled the tuning. [`Failover::stated`] is the whole point of the type:
//! a set of periods that do not hold together is refused, with the direction it
//! broke in and the neighbour it broke against.
//!
//! # What stays a constant, and why that is not an omission
//!
//! [`crate::LEASE_GUARD`] is not here. It is the margin the fence closes ahead
//! of the grant's expiry, covering the rate difference between two clocks nobody
//! synchronised, and its own documentation gives the reason it is not settable:
//! *a value an operator can lower to zero is a value somebody will lower to zero
//! the day a lease refuses a write they wanted*. Lowering it does not tune the
//! fence, it removes it.
//!
//! The staleness floor is not here either, and for the opposite reason: it is
//! **derived**, by [`Failover::staleness_floor`], from the awareness interval it
//! has always been derived from. It is a published promise — the bound the API
//! names back to a caller who asked for something tighter — so a floor read from
//! a constant while the enforcement reads the policy would be two answers to one
//! question, drifting apart with nothing failing.

use std::time::Duration;

use tessari_constants::{AWARENESS_SECONDS, CAMPAIGN_SECONDS, COLLECTION_SECONDS, ROUND_SECONDS};

use crate::error::{Error, Result};
use crate::lease::{GUARD, TTL};

/// How many round times of margin a leader needs before its fence shuts.
///
/// A candidate stands when its remaining writable window has shrunk to two of
/// these: a round that yields a lease dated from when it *opened* has to be in
/// hand before the old fence closes, and one round time lands exactly on the
/// fence with nothing left for a round that is refused, lost or slow.
///
/// It appears in two of the four relations below because it is one fact — the
/// campaign cadence has to fit inside this window, and the lease has to be long
/// enough for the window to exist at all.
const ROUNDS_OF_MARGIN: u32 = 2;

/// The shortest period this policy admits anywhere.
///
/// A cadence of zero is not a fast cadence, it is a spin; a lease of zero is
/// spent at the instant it is taken. One second is the floor because every
/// period here is a network round trip at minimum.
const SHORTEST_PERIOD: Duration = Duration::from_secs(1);

/// The periods that decide how quickly a cluster reacts to losing its leader.
///
/// Built only through [`Failover::stated`] or taken from [`Failover::DEFAULT`],
/// so a value in hand is one whose relations have been checked. The fields are
/// private for that reason and not for encapsulation's sake: a struct literal
/// would be a second way in that skips the constructor, and the failures these
/// relations prevent are exactly the ones nothing else reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failover {
    awareness: Duration,
    collection: Duration,
    round: Duration,
    campaign: Duration,
    lease: Duration,
}

impl Failover {
    /// The policy this build ships, taken from the constants it has always used.
    ///
    /// Deliberately a `const` rather than a function: it is the same five
    /// numbers the engine ran on before there was a policy at all, so a cluster
    /// that never sets one behaves exactly as it did.
    ///
    /// That it satisfies every relation below is asserted by a test rather than
    /// claimed here — it is the one assertion that catches a later edit to a
    /// constant breaking the invariant, which would otherwise be found by a
    /// cluster rather than by the suite.
    pub const DEFAULT: Self = Self {
        awareness: Duration::from_secs(AWARENESS_SECONDS),
        collection: Duration::from_secs(COLLECTION_SECONDS),
        round: Duration::from_secs(ROUND_SECONDS),
        campaign: Duration::from_secs(CAMPAIGN_SECONDS),
        lease: TTL,
    };

    /// A policy as an operator stated it, refused if the periods do not hold
    /// together.
    ///
    /// # Errors
    ///
    /// [`Error::FailoverPeriodTooShort`] when any period is under a second.
    ///
    /// [`Error::FailoverCampaignOutpaced`] when the campaign cadence is slower
    /// than the window it has to act inside — the **too large** direction.
    ///
    /// [`Error::FailoverLeaseTooShort`] when the lease leaves no instant at
    /// which a holder is both writable and not yet campaigning — the **too
    /// small** direction.
    ///
    /// [`Error::FailoverCollectionAboveFloor`] when the collection period is at
    /// or above the staleness floor this policy publishes — the **too large**
    /// direction.
    pub fn stated(
        awareness: Duration,
        collection: Duration,
        round: Duration,
        campaign: Duration,
        lease: Duration,
    ) -> Result<Self> {
        let stated = Self {
            awareness,
            collection,
            round,
            campaign,
            lease,
        };
        stated.holds_together()?;
        Ok(stated)
    }

    /// How often a node learns something about its peers.
    ///
    /// Failure direction: **too large** widens the staleness floor with it, so
    /// the tightest bound the API admits loosens without anyone asking. Too
    /// small spends bandwidth on being sure of itself and breaks nothing.
    #[must_use]
    pub const fn awareness(&self) -> Duration {
        self.awareness
    }

    /// How often a follower collects the records it does not hold.
    ///
    /// Its own period and not the awareness interval it equals by default,
    /// because the two fail differently: a missed greeting costs the freshness
    /// of a routing reading, a missed collection costs **data**.
    ///
    /// Failure direction: **too large** — at or above the staleness floor the
    /// node advertises a bound it cannot meet even when nothing is wrong, which
    /// is why that is the relation rather than a suggestion.
    #[must_use]
    pub const fn collection(&self) -> Duration {
        self.collection
    }

    /// How long a canvass of the voting members takes on this network.
    ///
    /// A deadline rather than a measurement — the number that moves the day a
    /// cluster spans a region, and everything derived from it moves with it.
    ///
    /// Failure direction: **too small** makes a canvass that had not finished
    /// look like one that failed, so a leader stands again while the first round
    /// is still in flight. Too large is caught by the two relations it appears
    /// in rather than by a bound of its own.
    #[must_use]
    pub const fn round(&self) -> Duration {
        self.round
    }

    /// How often a leader checks whether it is time to stand again.
    ///
    /// Failure direction: **too large** — see [`Error::FailoverCampaignOutpaced`].
    /// Too small costs nothing measurable, because the decision to stand is
    /// taken *before* any socket is opened: a frequent cadence is not a frequent
    /// canvass.
    #[must_use]
    pub const fn campaign(&self) -> Duration {
        self.campaign
    }

    /// How long a leadership grant is good for.
    ///
    /// Failure direction: **too small** — see [`Error::FailoverLeaseTooShort`].
    /// Too large is the cost of a failover: nothing may write until the old
    /// lease expires, so this period is the floor under how long an automatic
    /// failover can possibly take.
    #[must_use]
    pub const fn lease(&self) -> Duration {
        self.lease
    }

    /// The tightest staleness bound a read may ask for under this policy.
    ///
    /// One awareness interval to learn something, and one more to notice that we
    /// did not. Derived here rather than stored so that the bound named in a
    /// refusal and the bound enforced are the same arithmetic — a stored copy is
    /// a second answer that can be left behind when awareness moves.
    #[must_use]
    pub fn staleness_floor(&self) -> Duration {
        self.awareness.saturating_mul(ROUNDS_OF_MARGIN)
    }

    /// The four relations, checked in the order an operator meets them.
    ///
    /// Every multiplication saturates, and each saturation lands on the refusing
    /// side: a round time large enough to saturate its own doubling makes the
    /// lease floor [`Duration::MAX`], so the lease is refused rather than waved
    /// through by an overflow.
    fn holds_together(&self) -> Result<()> {
        for (field, stated) in [
            ("awareness", self.awareness),
            ("collection", self.collection),
            ("round", self.round),
            ("campaign", self.campaign),
            ("lease", self.lease),
        ] {
            if stated < SHORTEST_PERIOD {
                return Err(Error::FailoverPeriodTooShort { field, stated });
            }
        }

        let margin = self.round.saturating_mul(ROUNDS_OF_MARGIN);
        if self.campaign > margin {
            return Err(Error::FailoverCampaignOutpaced {
                campaign: self.campaign,
                round: self.round,
                ceiling: margin,
            });
        }

        let floor = GUARD.saturating_add(margin);
        if self.lease <= floor {
            return Err(Error::FailoverLeaseTooShort {
                lease: self.lease,
                guard: GUARD,
                round: self.round,
                floor,
            });
        }

        let staleness_floor = self.staleness_floor();
        if self.collection >= staleness_floor {
            return Err(Error::FailoverCollectionAboveFloor {
                collection: self.collection,
                awareness: self.awareness,
                floor: staleness_floor,
            });
        }

        Ok(())
    }
}

impl Default for Failover {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    /// The policy the build ships, spelled out field by field so a test can move
    /// one of them without the others following it.
    fn shipped() -> (Duration, Duration, Duration, Duration, Duration) {
        let policy = Failover::DEFAULT;
        (
            policy.awareness(),
            policy.collection(),
            policy.round(),
            policy.campaign(),
            policy.lease(),
        )
    }

    #[test]
    fn the_policy_this_build_ships_satisfies_every_relation() {
        // The assertion that makes the constants and the relations one thing. A
        // later edit that raises CAMPAIGN_SECONDS above twice ROUND_SECONDS is
        // caught here rather than by a cluster whose leader quietly stops
        // renewing.
        let (awareness, collection, round, campaign, lease) = shipped();
        Failover::stated(awareness, collection, round, campaign, lease)
            .expect("the shipped constants must hold together");
    }

    #[test]
    fn a_stated_policy_reads_back_exactly_what_was_stated() {
        let policy = Failover::stated(
            Duration::from_secs(30),
            Duration::from_secs(20),
            Duration::from_secs(3),
            Duration::from_secs(5),
            Duration::from_secs(40),
        )
        .expect("a policy inside every relation is accepted");

        assert_eq!(policy.awareness(), Duration::from_secs(30));
        assert_eq!(policy.collection(), Duration::from_secs(20));
        assert_eq!(policy.round(), Duration::from_secs(3));
        assert_eq!(policy.campaign(), Duration::from_secs(5));
        assert_eq!(policy.lease(), Duration::from_secs(40));
        // The floor moves WITH awareness, which is the whole reason it is
        // derived: a caller refused at 30s must be told 60s and not the 20s the
        // constant would still be reporting.
        assert_eq!(policy.staleness_floor(), Duration::from_secs(60));
    }

    #[test]
    fn a_period_of_zero_is_refused_and_named() {
        let (awareness, collection, _, campaign, lease) = shipped();
        let refused = Failover::stated(awareness, collection, Duration::ZERO, campaign, lease)
            .expect_err("a round time of zero is a spin, not a fast canvass");

        match refused {
            Error::FailoverPeriodTooShort { field, stated } => {
                assert_eq!(field, "round");
                assert_eq!(stated, Duration::ZERO);
            }
            other => panic!("expected the short-period refusal, got {other}"),
        }
    }

    #[test]
    fn a_campaign_cadence_slower_than_its_window_is_refused() {
        // The failure this refusal exists for: the leader steps over the moment
        // it should have stood, loses a lease it could have renewed, and nothing
        // reports an error because no round was ever attempted.
        let (awareness, collection, round, _, lease) = shipped();
        let outpaced = round
            .saturating_mul(ROUNDS_OF_MARGIN)
            .saturating_add(Duration::from_secs(1));
        let refused = Failover::stated(awareness, collection, round, outpaced, lease)
            .expect_err("a cadence slower than the window it must act inside is refused");

        match refused {
            Error::FailoverCampaignOutpaced {
                campaign,
                round: against,
                ceiling,
            } => {
                assert_eq!(campaign, outpaced);
                assert_eq!(against, round);
                assert_eq!(ceiling, round.saturating_mul(ROUNDS_OF_MARGIN));
            }
            other => panic!("expected the outpaced-campaign refusal, got {other}"),
        }
    }

    #[test]
    fn a_campaign_cadence_exactly_at_its_window_is_accepted() {
        // The boundary is inclusive on purpose: a cadence that lands exactly on
        // the window still leaves one whole round time for a retry, which is the
        // margin the window was sized for. Refusing it would make the relation
        // stricter than the arithmetic behind it.
        let (awareness, collection, round, _, lease) = shipped();
        Failover::stated(
            awareness,
            collection,
            round,
            round.saturating_mul(ROUNDS_OF_MARGIN),
            lease,
        )
        .expect("a cadence exactly at the window is inside the relation");
    }

    #[test]
    fn a_lease_with_no_writable_instant_is_refused() {
        let (awareness, collection, round, campaign, _) = shipped();
        let floor = GUARD.saturating_add(round.saturating_mul(ROUNDS_OF_MARGIN));
        let refused = Failover::stated(awareness, collection, round, campaign, floor)
            .expect_err("a lease at the floor leaves no instant that is writable and not standing");

        match refused {
            Error::FailoverLeaseTooShort {
                lease,
                guard,
                round: against,
                floor: named,
            } => {
                assert_eq!(lease, floor);
                assert_eq!(guard, GUARD);
                assert_eq!(against, round);
                assert_eq!(named, floor);
            }
            other => panic!("expected the short-lease refusal, got {other}"),
        }
    }

    #[test]
    fn a_collection_period_at_the_staleness_floor_is_refused() {
        let (awareness, _, round, campaign, lease) = shipped();
        let floor = awareness.saturating_mul(ROUNDS_OF_MARGIN);
        let refused = Failover::stated(awareness, floor, round, campaign, lease)
            .expect_err("a collection period at the floor advertises a bound nothing can meet");

        match refused {
            Error::FailoverCollectionAboveFloor {
                collection,
                awareness: against,
                floor: named,
            } => {
                assert_eq!(collection, floor);
                assert_eq!(against, awareness);
                assert_eq!(named, floor);
            }
            other => panic!("expected the collection-above-floor refusal, got {other}"),
        }
    }

    #[test]
    fn widening_awareness_widens_the_floor_the_collection_is_judged_against() {
        // The relation is not a constant in disguise. A collection period that
        // is refused under the shipped awareness is accepted once awareness is
        // widened, because the floor it is judged against moved with it — which
        // is what makes the pair tunable rather than merely settable.
        let (awareness, _, round, campaign, lease) = shipped();
        let collection = awareness.saturating_mul(ROUNDS_OF_MARGIN);

        Failover::stated(awareness, collection, round, campaign, lease)
            .expect_err("refused against the shipped awareness");
        Failover::stated(
            awareness.saturating_mul(ROUNDS_OF_MARGIN),
            collection,
            round,
            campaign,
            lease,
        )
        .expect("accepted once the floor it is judged against moved");
    }

    #[test]
    fn a_round_time_large_enough_to_saturate_refuses_the_lease_rather_than_admitting_it() {
        // The overflow direction, asserted rather than reasoned about: doubling
        // a round time near the maximum saturates, the lease floor becomes
        // unreachable, and the refusal is the safe answer.
        //
        // The campaign cadence here is a plain second and not the same maximum,
        // and that is the point rather than an arbitrary choice. Stating the
        // maximum for both made the refusal depend on which relation is checked
        // FIRST — two deliberate mutations of the campaign boundary turned this
        // test red while leaving the lease arithmetic it claims to cover
        // untouched. A test whose verdict moves with the check order is not
        // testing the check.
        let (awareness, collection, _, _, _) = shipped();
        let enormous = Duration::MAX;
        let refused = Failover::stated(
            awareness,
            collection,
            enormous,
            Duration::from_secs(1),
            enormous,
        )
        .expect_err("a saturated floor refuses even the longest lease expressible");

        assert!(
            matches!(refused, Error::FailoverLeaseTooShort { .. }),
            "expected the short-lease refusal from the saturated floor, got {refused}"
        );
    }
}
