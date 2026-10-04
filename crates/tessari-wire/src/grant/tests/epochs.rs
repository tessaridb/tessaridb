use super::*;

#[test]
fn two_candidates_cannot_both_carry_one_epoch() {
    let now = base();
    let mut voters = [settled(now), settled(now), settled(now)];

    let mut first = Round::opened_at(Epoch::new(1), A, voters.len(), now);
    let mut held = None;
    for (voter, id) in voters.iter_mut().zip([ONE, TWO, THREE]) {
        let vote = voter.asked(&first.ballot(), now, LEVEL, LEVEL);
        held = first.counts(id, vote);
    }
    assert!(
        held.is_some(),
        "three willing voters carry a round of three"
    );

    // The second candidate asks the same epoch of the same voters, a
    // moment later, and every one of them has already decided it.
    let later = after(now, Duration::from_millis(1));
    let mut second = Round::opened_at(Epoch::new(1), B, voters.len(), later);
    for (voter, id) in voters.iter_mut().zip([ONE, TWO, THREE]) {
        let vote = voter.asked(&second.ballot(), later, LEVEL, LEVEL);
        assert_eq!(
            vote,
            Vote::Refused(Refused::EpochAlreadyDecided {
                granted: Epoch::new(1)
            })
        );
        second.counts(id, vote);
    }
    assert_eq!(second.held(), None, "one epoch, one leader");
}

#[test]
fn a_voter_grants_one_epoch_to_one_candidate_however_long_it_waits() {
    // **Narrowed in W228, deliberately, and this comment is the record.**
    // This asserted *an epoch at most once*, which is stronger than the
    // property it was protecting: what carries the safety is one epoch, one
    // CANDIDATE — `two_candidates_cannot_both_carry_one_epoch`, untouched.
    // The stronger reading also made renewal impossible, because a renewal
    // re-asks its own epoch (only an election advances one, since the log's
    // divergence check reads an epoch as a leadership generation).
    //
    // So the refusal is asserted against a DIFFERENT candidate, and the same
    // one is asserted to be granted. Both long after the first grant has
    // expired, so nothing but the epoch rule itself can be doing either.
    let now = base();
    let mut voter = settled(now);
    let ballot = Ballot {
        epoch: Epoch::new(7),
        candidate: A,
        range: tessari_types::Reach::Store,
    };

    assert_eq!(
        voter.asked(&ballot, now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );

    let long_after = after(now, LEASE_TTL.saturating_add(Duration::from_secs(60)));
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(7),
                candidate: B,
                range: tessari_types::Reach::Store,
            },
            long_after,
            LEVEL,
            LEVEL
        ),
        Vote::Refused(Refused::EpochAlreadyDecided {
            granted: Epoch::new(7)
        })
    );
    assert_eq!(
        voter.asked(&ballot, long_after, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL },
        "the holder re-asking its own epoch adds no second holder"
    );
    assert_eq!(voter.decided(), Some(Epoch::new(7)));
}

#[test]
fn an_incumbent_may_renew_before_the_lease_it_holds_expires() {
    // The hole this wave exists to close. A leader has to renew strictly
    // before its own fence shuts, which is `LEASE_GUARD` before the lease
    // expires — and the voter's hold runs to the expiry itself, so every
    // renewal that is not already too late arrives inside a window the
    // voter is still holding.
    //
    // Re-granting to the node that already holds it produces one holder,
    // which is the whole of the property `EarlierGrantStillAlive` protects.
    let now = base();
    let mut voter = settled(now);
    let ballot = Ballot {
        epoch: Epoch::new(4),
        candidate: A,
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        voter.asked(&ballot, now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );

    // The last moment a renewal is any use: one instant before the holder
    // stops writing. The voter is still holding for `LEASE_GUARD` longer.
    let renewing = after(now, LEASE_TTL.saturating_sub(LEASE_GUARD));
    assert_eq!(
        voter.asked(&ballot, renewing, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL },
        "a leader that cannot renew before its own fence holds a terminal lease"
    );
}

#[test]
fn a_renewal_moves_the_window_the_next_challenger_waits_out() {
    // A renewal is a grant, so the voter's own hold is measured from it. A
    // renewal that refreshed the holder without refreshing the voter would
    // free the voter while the lease it had just extended was alive, which
    // is the split-brain the guard exists to prevent, arriving by the one
    // door this wave opens.
    let now = base();
    let mut voter = settled(now);
    let ballot = Ballot {
        epoch: Epoch::new(4),
        candidate: A,
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        voter.asked(&ballot, now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );

    let renewed = after(now, LEASE_TTL.saturating_sub(LEASE_GUARD));
    assert_eq!(
        voter.asked(&ballot, renewed, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(
        voter.free_at(),
        Some(after(renewed, LEASE_TTL)),
        "the voter is free one TTL after the renewal, not after the first grant"
    );
}

#[test]
fn a_challenger_cannot_take_the_epoch_its_holder_is_still_renewing() {
    // The other half, and the reason the candidate has to be compared rather
    // than the epoch alone: B asking for A's live epoch is the impersonation
    // case with the name left off.
    let now = base();
    let mut voter = settled(now);
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(4),
                candidate: A,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            LEVEL
        ),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(4),
                candidate: B,
                range: tessari_types::Reach::Store,
            },
            after(now, tenths(1)),
            LEVEL,
            LEVEL
        ),
        Vote::Refused(Refused::EpochAlreadyDecided {
            granted: Epoch::new(4)
        })
    );
}
