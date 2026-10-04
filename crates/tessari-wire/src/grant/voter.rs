use super::*;

impl Voter {
    /// A voter that started now.
    #[must_use]
    pub fn started() -> Self {
        Self::started_at(Instant::now())
    }

    /// The epoch of the grant this voter holds, if it holds one.
    pub(crate) fn granted_epoch(&self) -> Option<Epoch> {
        self.granted.map(|granted| granted.epoch)
    }

    /// A voter that started at a stated instant.
    ///
    /// The instant is taken rather than read so that the restart rule can be
    /// exercised without a clock, and therefore without a test that can flake.
    #[must_use]
    pub const fn started_at(started: Instant) -> Self {
        Self {
            started,
            hold: LEASE_TTL,
            granted: None,
            seen: Epoch::ZERO,
        }
    }

    /// The same voter, holding every grant for `hold`.
    #[must_use]
    pub const fn holding_for(mut self, hold: Duration) -> Self {
        self.hold = hold;
        self
    }

    /// Hold every grant made from now on for `hold` — the policy the store has
    /// installed since. A grant already made keeps the hold it was made for.
    pub const fn hold_for(&mut self, hold: Duration) {
        self.hold = hold;
    }

    /// Answer one ballot.
    ///
    /// The checks are ordered by how different their remedies are. A log behind
    /// this voter's own is first because it is the only refusal that does not
    /// depend on what this voter has done — it is a statement about the
    /// candidate, true whatever this voter granted and whenever it started, and
    /// a candidate told to catch up has something to do about it. Then: a
    /// repeated epoch is a confused candidate; a live grant is the normal answer
    /// and the caller wants to know how long to wait; a recent start is a node
    /// sitting out a lease it cannot remember.
    ///
    /// # Both positions are arguments, and neither is read from a frame
    ///
    /// `mine` is this node's own log position and `candidate` is the position
    /// the candidate **proved** when it greeted — not one it wrote into the
    /// ballot. It is the rule [`crate::Error::NotItsOwnBallot`] already applies
    /// to the candidate's identity, for the same reason: a fact a candidate
    /// states about itself in the frame being judged is a fact it can choose,
    /// and an election restriction a candidate can opt out of restricts nothing.
    ///
    /// They are arguments rather than fields because a log position changes with
    /// every commit, so a voter that remembered one would be answering from a
    /// picture the store has already moved past.
    pub fn asked(
        &mut self,
        ballot: &Ballot,
        now: Instant,
        mine: Reached,
        candidate: Reached,
    ) -> Vote {
        // The incumbent renewing the epoch this voter's tail was written under
        // holds that tail by construction: one epoch has one leader, and it
        // wrote every entry under it. Judged on a greeting read before the
        // handshake, it would look behind the entries it streamed here since
        // (G057 SG6) — Raft never puts a leader's heartbeat to the election
        // restriction either.
        let renewing = self
            .granted
            .is_some_and(|held| held.candidate == ballot.candidate && held.epoch == ballot.epoch)
            && mine.leadership == ballot.epoch;
        if !renewing && candidate.behind(mine) {
            return Vote::Refused(Refused::LogBehind {
                leadership: mine.leadership,
                tail: mine.tail,
            });
        }
        // Judged against what was seen BEFORE this ballot, and adopted only
        // past the live-grant check below (Q-880, Raft's leader stickiness,
        // thesis §4.2.3). Adopting on every ballot let a challenger this voter
        // REFUSED for a live grant end that grant: the incumbent's renewal then
        // read as an epoch already decided, and a lease nobody had taken was
        // lost. Epoch order does not need it — a higher epoch that won was
        // granted by a majority, each member adopted it as it granted, and any
        // majority for a lower epoch includes one of them.
        let seen = self.seen;
        // Before the waiting refusals below, for the reason `LogBehind` is
        // first: a candidate told that the cluster has passed it has something
        // to do about it, where *wait six seconds* leaves it to stand again at
        // the same stale number.
        if ballot.epoch < seen {
            return Vote::Refused(Refused::EpochAlreadyDecided { granted: seen });
        }
        if let Some(held) = self.granted {
            // The node this voter is already holding a grant for. Both rules
            // below turn on it, and neither is safe without the **proved**
            // identity behind it — `Link::greet` refuses a ballot naming anyone
            // but the peer that presented the credential, because from here a
            // claimed name is indistinguishable from a true one.
            let incumbent = ballot.candidate == held.candidate;
            // A renewal re-asks its own epoch, so `<=` would refuse every one of
            // them. The invariant safety actually needs is *one epoch, one
            // CANDIDATE* — `two_candidates_cannot_both_carry_one_epoch` — and
            // that is what this says. An epoch below the one held is somebody
            // working from a stale picture whoever they are.
            // The `<` half can no longer be reached — a granted epoch is a
            // seen epoch, so anything below it was refused above — and it stays
            // because `seen` is in-memory hygiene while this is the authority:
            // the day the two are made to disagree, the rule that matters is
            // still written where the grant is.
            if ballot.epoch < held.epoch || (ballot.epoch == held.epoch && !incumbent) {
                return Vote::Refused(Refused::EpochAlreadyDecided {
                    granted: held.epoch,
                });
            }
            let free = free_at(held.at, held.hold);
            if !incumbent && now < free {
                return Vote::Refused(Refused::EarlierGrantStillAlive {
                    for_the_next: free.saturating_duration_since(now),
                });
            }
        }
        // Past the live grant, the ballot is one this voter could grant, and its
        // epoch is adopted whatever comes next — including the restart refusal
        // below: a restarted voter that forgot a grant must still not go back to
        // granting below what it has been shown (G025 S3.2).
        self.seen = seen.max(ballot.epoch);
        if self.granted.is_none() {
            // Granted nothing since it started, so it cannot rule out having
            // granted something before it started.
            // The longer of the policy and the build: what this voter granted
            // before it restarted was held for the policy it ran under, which
            // the store still carries — and a policy shorter than the build's
            // must not shorten the window a build-length grant needs.
            let settled = free_at(self.started, self.hold.max(LEASE_TTL));
            if now < settled {
                return Vote::Refused(Refused::TooSoonAfterStarting {
                    for_the_next: settled.saturating_duration_since(now),
                });
            }
        }

        // `now` and not the earlier grant's instant, including on a renewal:
        // the voter's own hold runs from the grant it most recently made, or it
        // would come free while the lease it had just extended was still alive.
        self.granted = Some(Granted {
            epoch: ballot.epoch,
            candidate: ballot.candidate,
            at: now,
            hold: self.hold,
        });
        Vote::Granted { hold: self.hold }
    }

    /// Take a round a majority carried for `ballot`'s candidate — this node —
    /// into this voter's memory, whether or not this voter granted it; or
    /// refuse it, answering the higher epoch this voter has already granted.
    ///
    /// # A win this memory does not hold is a leader its own voter disowns
    ///
    /// The self-vote is cast last and can be refused: a voter that granted a
    /// rival an epoch moments ago refuses this node the next one for a whole
    /// TTL, and the peers carry the round without it. The win is sound — every
    /// voter in that majority kept its own promise, so the rival's lease cannot
    /// be alive — but the memory still names the rival. Read back as a grant to
    /// somebody else, it silenced the leader at its own standing gate for the
    /// rest of the lease, and once that grant aged out the same memory would
    /// grant the NEXT epoch to a challenger while this node still wrote under
    /// this one (G053 SG2d).
    ///
    /// A rival's grant of the SAME epoch is replaced too. Every voter grants an
    /// epoch once, so a majority that carried it for this node means the
    /// rival's bid at it lost; the record names a candidacy known to be over,
    /// and replacing it only ever refuses more — the rival is no longer the
    /// incumbent, and the hold runs from now.
    ///
    /// # A grant ABOVE the round ends the round
    ///
    /// A ballot this node's door granted while the round was in flight carried
    /// this node's log position as it then stood: the rival was promised it
    /// needed nothing after it. Leading the lower epoch would append past that
    /// promise — records acknowledged here and absent from the line a winning
    /// rival keeps. Raft's rule, for the same reason: a node that voted in a
    /// higher term is a follower in it, and an election for a lower term is
    /// over whatever its replies say. Decided under the one lock the door also
    /// takes, so no grant can land between the check and the record.
    ///
    /// # Errors
    ///
    /// The epoch this voter granted above the round, which the caller stands
    /// past next time.
    pub fn carried(&mut self, ballot: &Ballot, now: Instant) -> std::result::Result<(), Epoch> {
        if let Some(held) = self.granted
            && held.epoch > ballot.epoch
        {
            return Err(held.epoch);
        }
        self.seen = self.seen.max(ballot.epoch);
        self.granted = Some(Granted {
            epoch: ballot.epoch,
            candidate: ballot.candidate,
            at: now,
            hold: self.hold,
        });
        Ok(())
    }

    /// The highest epoch this voter has granted, if any.
    #[must_use]
    pub fn decided(&self) -> Option<Epoch> {
        self.granted.map(|held| held.epoch)
    }

    /// When this voter last granted a ballot **to a node that is not itself**,
    /// which is when it last had evidence that a leader was alive.
    ///
    /// # The `me` is the whole of it, and leaving it out cost a wave
    ///
    /// A node keeps ONE voting memory, shared by the peer door and by its own
    /// campaign, so that it cannot grant a single epoch twice. It follows that a
    /// candidate's self-vote lands here exactly like a peer's ballot — and until
    /// W275 this function reported it, so the standing gate read a node's own
    /// vote as proof that a leader was alive. The act of standing set the flag
    /// that forbids standing, for exactly `LEASE_TTL`: a leader was silenced for
    /// precisely as long as the lease it was trying to renew, and a lease could
    /// only be re-won after it had already been lost. Measured at W274 in a
    /// three-process run — two ten-second silences bracketing one round, one of
    /// them a sitting leader.
    ///
    /// The candidate was in the record the whole time and nothing read it. The
    /// argument for comparing it here rather than at the one call site is the
    /// one this tree applies everywhere else: a rule a caller has to remember is
    /// a rule that holds until the next caller.
    ///
    /// A node that voted for itself has heard nobody. That is not a refusal to
    /// answer — it is the answer.
    ///
    /// # It is the freshest liveness signal this node has, and it was already here
    ///
    /// A leader renews by putting a ballot to every voter at once, and it
    /// renews while two round times are left of its usable window — so a voter
    /// hears from a live leader roughly every `LEASE_TTL - LEASE_GUARD - 2 ×
    /// ROUND_MILLIS` plus one campaign tick, which is **about 300 ms** at
    /// today's values. The greeting directory that [`crate::heard_a_leader`]
    /// otherwise consults is refreshed on the awareness cadence, **a second**,
    /// and the reading itself is up to that old again. This instant is strictly
    /// fresher and costs nothing: the grant was already recorded, with `now` and
    /// not the earlier instant, precisely so that a renewal moves it.
    ///
    /// # A refusal is not evidence
    ///
    /// Only a grant is reported. Refusing a ballot says this voter would not
    /// have that node as leader — a candidate standing against a leader that is
    /// already gone refuses nothing and proves nothing about the leader. Reading
    /// a refusal as contact would let a dead cluster keep itself quiet by
    /// arguing with itself.
    #[must_use]
    pub fn granted_elsewhere_at(&self, me: [u8; NODE_ID_LEN]) -> Option<Instant> {
        self.granted
            .filter(|held| held.candidate != me)
            .map(|held| held.at)
    }

    /// The highest epoch this voter has been shown, granted or refused.
    #[must_use]
    pub const fn seen(&self) -> Epoch {
        self.seen
    }

    /// When this voter is next free to grant, if it has granted at all.
    ///
    /// The grantor's **expiry**, not the holder's fence: the guard is exactly the
    /// difference between the two, and it belongs on this side of the pair.
    #[must_use]
    pub fn free_at(&self) -> Option<Instant> {
        self.granted.map(|held| free_at(held.at, held.hold))
    }
}
