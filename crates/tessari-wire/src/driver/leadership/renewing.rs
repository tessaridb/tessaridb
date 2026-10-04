use super::*;

/// The leadership this node holds, and what a lost round does to it.
///
/// # A lost round has to leave a mark, or it is not a round
///
/// ADR-0066. Until W257 this held one field — the leadership won — and derived
/// the next epoch from it. So a candidate that lost stood for the same epoch
/// again, against voters that had already spent it, for the life of the
/// process: three nodes started together elected nobody in ninety seconds, all
/// healthy, nothing in an error state. Losing is the ordinary case in an
/// election and it needs its own memory.
///
/// Three facts, because they answer three different questions. What this node
/// **holds** decides whether it may write. What it has **stood for** stops it
/// re-asking an epoch it has already spent. What it has **heard granted** stops
/// it climbing one epoch per round towards a cluster that is already far above
/// it — a node that never led holds [`Epoch::ZERO`] however long the cluster
/// has been running, and every refusal it gets is already carrying the number.
#[derive(Debug)]
pub struct Renewing {
    standing: Leadership,
    stood: Epoch,
    heard: Epoch,
    /// When this node may open its next round, after losing one.
    ///
    /// `None` means *now*. See [`Renewing::stagger`] for why a fixed wait is not
    /// enough and what this is derived from.
    not_before: Option<Instant>,
}

impl Renewing {
    /// Hold `standing` until something better is won.
    #[must_use]
    pub fn holding(standing: Leadership) -> Self {
        Self {
            stood: standing.epoch,
            standing,
            heard: Epoch::ZERO,
            not_before: None,
        }
    }

    /// The leadership this node is currently standing on.
    #[must_use]
    pub fn standing(&self) -> Leadership {
        self.standing
    }

    /// One renewal round: the epoch held, while nothing says the cluster has
    /// moved past it; otherwise the epoch after everything this node has seen.
    ///
    /// # A renewal keeps its epoch
    ///
    /// A leader renews about every 300 ms (G053 SG2b), and a new epoch per
    /// renewal is a new leadership record per renewal — three a second on an
    /// idle cluster, each one a commit every follower applies, together eating
    /// the retained log in hours. The voter admits the incumbent re-asking its
    /// own epoch, and safety is unchanged: a voter still grants one epoch to one
    /// candidate, so two majorities for one epoch still need a voter that
    /// granted it twice. A fresh epoch is stood for when this node holds none,
    /// when it already stood above what it holds, or when a refusal named a
    /// higher one — each a sign that re-asking the held epoch would be refused.
    ///
    /// # Why a round that wins nothing changes nothing
    ///
    /// [`crate::Standing::renew`] answers `None` in two circumstances: there was
    /// margin left and nobody was asked, or a round ran and no majority granted
    /// it. Both mean *carry on with the lease you have*, and the first is the
    /// ordinary case — a cadence that ran a little early. Standing down on
    /// `None` would make a healthy leader resign because its timer fired before
    /// its fence needed defending.
    pub fn once(
        &mut self,
        candidate: [u8; NODE_ID_LEN],
        now: Instant,
        pass: impl FnOnce(Lease, Epoch) -> Stood,
    ) -> Leadership {
        if self.not_before.is_some_and(|until| now < until) {
            return self.standing;
        }
        let held = self.standing.epoch;
        let next = if held > Epoch::ZERO && self.stood == held && self.heard <= held {
            held
        } else {
            Epoch::new(
                held.get()
                    .max(self.stood.get())
                    .max(self.heard.get())
                    .saturating_add(1),
            )
        };
        match pass(self.standing.lease(), next) {
            // Untouched, and that is the ordinary case: a cadence that ran a
            // little early while the fence was still far off. Standing down here
            // would make a healthy leader resign because its timer fired.
            Stood::NotDue => {}
            Stood::Won(won) => {
                self.standing = won;
                self.stood = won.epoch;
                self.not_before = None;
            }
            Stood::Lost { granted } => {
                self.stood = next;
                if granted > self.heard {
                    self.heard = granted;
                }
                // `None` on an instant so far out that the addition cannot be
                // represented, which reads as *stand now*. That is the safe
                // direction: an unrepresentable clock should not be able to
                // stop a node standing for an epoch.
                self.not_before = now.checked_add(Self::stagger(candidate, next));
            }
        }
        self.standing
    }

    /// How long this node waits before standing again, having just lost.
    ///
    /// # Why a wait at all
    ///
    /// Advancing the epoch alone does not break a tie. Three candidates that
    /// lose together stand for the next epoch on the same tick, each grants it
    /// to itself, each refuses the other two, and the deadlock repeats one epoch
    /// higher forever. Raft states the randomised election timeout as the
    /// liveness argument itself rather than as a tuning detail, and this is why.
    ///
    /// # Why it is derived rather than random
    ///
    /// The property needed is only that two candidates do not retry in step. A
    /// value derived from the candidate gives that, and gives one thing a random
    /// one cannot: a test may state the instant a node will stand and assert it,
    /// where randomness can only be observed not to deadlock over some number of
    /// runs. It also keeps this crate's dependency list as it is.
    ///
    /// **The epoch is in the mix and that is what makes it as strong as
    /// randomness.** Two nodes whose ids happen to fall close together collide
    /// at one epoch and not at the next; derived from the id alone, an unlucky
    /// pair would collide at every epoch forever — which is the failure this
    /// exists to remove, reintroduced one level down.
    pub(crate) fn stagger(candidate: [u8; NODE_ID_LEN], epoch: Epoch) -> Duration {
        spread(candidate, epoch, tessari_constants::ROUND_MILLIS)
    }
}

/// How long this node goes without hearing a leader before it stands: the
/// lease, plus a spread derived from this node and the last epoch it granted.
///
/// # Why the lease and not less
///
/// A voter refuses everyone but the incumbent until a whole lease after its last
/// grant ([`crate::Voter`]), so standing sooner only spends rounds that cannot
/// win. Standing later than the lease is the whole failover budget going on
/// waiting, which is what the spread is kept small for.
///
/// # Why a spread at all
///
/// Every voter hears the same renewal ballot, so their memories of a dead
/// leader lapse within milliseconds of each other. Two followers standing on
/// the same tick each grant the other the one epoch and both lose it for a whole
/// lease; Raft's randomised election timeout is the liveness argument for
/// exactly this. Derived rather than random for the reason [`Renewing::stagger`]
/// gives — a test can state when a node stands — and with the epoch in the mix
/// so that two ids that collide once do not collide at the next election.
///
/// `lease` is the hold this node's own voter grants for — the installed failover
/// policy's lease (G053 SG2c). A leader's lease is never longer than the hold of
/// a voter that granted it, so a node whose grant has aged past its own hold has
/// outlived any lease that grant could have carried.
#[must_use]
pub fn election_timeout(me: [u8; NODE_ID_LEN], granted: Epoch, lease: Duration) -> Duration {
    lease.saturating_add(spread(
        me,
        granted,
        tessari_constants::ELECTION_JITTER_MILLIS,
    ))
}

/// A duration in `[0, millis)` ms, derived from a node and an epoch.
pub(super) fn spread(candidate: [u8; NODE_ID_LEN], epoch: Epoch, millis: u64) -> Duration {
    // A 64-bit mix of the id and the epoch. The constants are SplitMix64's;
    // nothing here needs a distribution better than "two different inputs land
    // in different places", and a named mixer is easier to recognise than an
    // invented one.
    let mut mixed = epoch.get();
    for chunk in candidate.chunks(8) {
        let mut byte = 0_u64;
        for (place, value) in chunk.iter().enumerate() {
            byte |= u64::from(*value) << (place.saturating_mul(8));
        }
        mixed ^= byte;
        mixed = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^= mixed >> 31;
    }
    Duration::from_millis(mixed.rem_euclid(millis.max(1)))
}
