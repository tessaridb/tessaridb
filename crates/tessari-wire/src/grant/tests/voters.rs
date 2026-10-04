use super::*;

#[test]
fn a_grant_moves_the_instant_a_leader_is_judged_alive_by_and_a_refusal_does_not() {
    let now = base();
    let mut voter = settled(now);
    // Asked as C throughout: every grant below is to somebody else, which is
    // the case this test has always been about. The self-grant is the test
    // underneath this one.
    assert_eq!(
        voter.granted_elsewhere_at(C),
        None,
        "nothing granted, nothing to read"
    );

    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(1),
                candidate: A,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            LEVEL
        ),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(voter.granted_elsewhere_at(C), Some(now));

    // A renewal from the incumbent moves it, because that is the whole
    // mechanism: a leader renews about every 300 ms and this is how a
    // voter knows the leader was alive that recently.
    let later = after(now, tenths(6));
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(1),
                candidate: A,
                range: tessari_types::Reach::Store,
            },
            later,
            LEVEL,
            LEVEL
        ),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(voter.granted_elsewhere_at(C), Some(later));

    // A REFUSAL does not. B standing against the live incumbent is refused,
    // and refusing says this voter would not have B as leader — it is no
    // evidence at all that any leader is alive. Reading it as contact would
    // let a cluster whose leader is long gone keep itself quiet by arguing
    // with itself.
    let refused_at = after(later, tenths(1));
    assert!(matches!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(2),
                candidate: B,
                range: tessari_types::Reach::Store,
            },
            refused_at,
            LEVEL,
            LEVEL
        ),
        Vote::Refused(_)
    ));
    assert_eq!(
        voter.granted_elsewhere_at(C),
        Some(later),
        "a refusal moved the instant a leader is judged alive by"
    );
}

#[test]
fn a_node_that_voted_for_itself_has_heard_nobody() {
    // The assertion W273 did not have, and the whole of Q-602. A candidate
    // self-votes through this same memory — one voting memory per node, so
    // that a node cannot grant one epoch twice — so its own ballot is
    // indistinguishable from a peer's unless the candidate is compared.
    //
    // Read the other way it would silence the only node that must not be
    // silenced: a leader renews by standing, standing self-votes, and a
    // self-vote read as a leader's liveness stops the next renewal for a
    // whole `LEASE_TTL` — exactly the lease being renewed.
    let now = base();
    let mut voter = settled(now);

    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(1),
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
        voter.granted_elsewhere_at(A),
        None,
        "a node read its own vote as evidence that a leader was alive"
    );
    // And the same grant, asked about by anybody else, still answers — the
    // filter is about who asked, not about forgetting the grant.
    assert_eq!(voter.granted_elsewhere_at(B), Some(now));
    assert_eq!(voter.granted_elsewhere_at(C), Some(now));
}

#[test]
fn a_voter_will_not_grant_again_while_the_grant_it_made_may_be_alive() {
    let now = base();
    let mut voter = settled(now);
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(1),
                candidate: A,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            LEVEL
        ),
        Vote::Granted { hold: LEASE_TTL }
    );

    let soon = after(now, tenths(1));
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(2),
                candidate: B,
                range: tessari_types::Reach::Store,
            },
            soon,
            LEVEL,
            LEVEL
        ),
        Vote::Refused(Refused::EarlierGrantStillAlive {
            for_the_next: LEASE_TTL.saturating_sub(tenths(1))
        })
    );

    // Once its own hold has run out it is free, and it says so at exactly
    // the expiry rather than at the holder's fence — the guard is the gap
    // between those two and it belongs on this side.
    assert_eq!(voter.free_at(), Some(after(now, LEASE_TTL)));
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(2),
                candidate: B,
                range: tessari_types::Reach::Store,
            },
            after(now, LEASE_TTL),
            LEVEL,
            LEVEL
        ),
        Vote::Granted { hold: LEASE_TTL }
    );
}

#[test]
fn a_voter_that_has_just_started_sits_out_one_lease() {
    let started = base();
    let mut voter = Voter::started_at(started);
    let ballot = Ballot {
        epoch: Epoch::new(1),
        candidate: A,
        range: tessari_types::Reach::Store,
    };

    let early = after(started, tenths(4));
    assert_eq!(
        voter.asked(&ballot, early, LEVEL, LEVEL),
        Vote::Refused(Refused::TooSoonAfterStarting {
            for_the_next: LEASE_TTL.saturating_sub(tenths(4))
        })
    );
    assert_eq!(voter.decided(), None, "a refusal decides nothing");

    assert_eq!(
        voter.asked(&ballot, after(started, LEASE_TTL), LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
}

#[test]
fn a_voter_adopts_an_epoch_it_refused_and_will_not_grant_below_it_afterwards() {
    // G025 S3.2, and the hole it closes is entirely inside the restart
    // window. `a_voter_that_has_just_started_sits_out_one_lease` above
    // asserts the refusal; what it cannot see is that the refusal used to
    // throw the NUMBER away with the ballot.
    let started = base();
    let mut voter = Voter::started_at(started);

    let high = Ballot {
        epoch: Epoch::new(9),
        candidate: A,
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        voter.asked(&high, after(started, tenths(1)), LEVEL, LEVEL),
        Vote::Refused(Refused::TooSoonAfterStarting {
            for_the_next: LEASE_TTL.saturating_sub(tenths(1))
        })
    );
    assert_eq!(voter.decided(), None, "a refusal grants nothing");
    assert_eq!(
        voter.seen(),
        Epoch::new(9),
        "and it keeps the number regardless, because an epoch is the              cluster's count and not this voter's"
    );

    // One whole TTL later the start guard is spent and this voter may grant
    // again. Before the adoption it granted THIS — an epoch four below one
    // it had already been shown, to a candidate working from a stale
    // picture, with nothing in an error state.
    let low = Ballot {
        epoch: Epoch::new(5),
        candidate: B,
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        voter.asked(&low, after(started, LEASE_TTL), LEVEL, LEVEL),
        Vote::Refused(Refused::EpochAlreadyDecided {
            granted: Epoch::new(9)
        })
    );
    assert_eq!(voter.decided(), None, "and still nothing has been granted");

    // The refusal is actionable rather than merely safe: it carries the
    // number, and a candidate that catches up to it wins.
    let caught_up = Ballot {
        epoch: Epoch::new(10),
        candidate: B,
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        voter.asked(&caught_up, after(started, LEASE_TTL), LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
}

#[test]
fn an_epoch_refused_for_a_live_grant_does_not_end_that_grant() {
    // Q-880, Raft's leader stickiness (thesis §4.2.3). A voter holding a
    // live grant for A refuses a challenger at 9 — and until G053 SG2c it
    // adopted 9 as it refused, then refused A's own renewal at 3 as an
    // epoch already decided. A lease nobody had taken was lost, and a
    // leader-only acknowledgement with it (run 42, link 4).
    //
    // Epoch order does not need the adoption: a 9 that WON was granted by
    // a majority, every member of which adopted 9 as it granted, and any
    // majority for a lower epoch includes one of them.
    let opened = base();
    let mut voter = settled(opened);
    let ballot = |epoch: u64, candidate| Ballot {
        epoch: Epoch::new(epoch),
        candidate,
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        voter.asked(&ballot(3, A), opened, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(
        voter.asked(&ballot(9, B), opened, LEVEL, LEVEL),
        Vote::Refused(Refused::EarlierGrantStillAlive {
            for_the_next: LEASE_TTL
        }),
        "the hold it made for A is still alive, so 9 is refused"
    );
    assert_eq!(
        voter.asked(&ballot(3, A), after(opened, tenths(3)), LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL },
        "the refused challenger ended the incumbent's renewal"
    );
    assert_eq!(
        voter.seen(),
        Epoch::new(3),
        "a refused ballot raised the floor"
    );
}

#[test]
fn the_holder_stops_writing_before_the_earliest_voter_is_free_again() {
    let opened = base();
    let mut voters = [settled(opened), settled(opened), settled(opened)];
    let mut round = Round::opened_at(Epoch::new(3), A, voters.len(), opened);

    // A round that drags: one voter answers at once, one after a tenth of
    // the lease, one after three tenths — longer than the guard, which is the
    // only shape in which dating the lease wrongly is detectable.
    let answered = [opened, after(opened, tenths(1)), after(opened, tenths(3))];
    let mut held = None;
    for ((voter, id), at) in voters.iter_mut().zip([ONE, TWO, THREE]).zip(answered) {
        let vote = voter.asked(&round.ballot(), at, LEVEL, LEVEL);
        assert_eq!(vote, Vote::Granted { hold: LEASE_TTL });
        held = round.counts(id, vote);
    }

    let held: Leadership = held.expect("three of three carried it");
    let earliest_free = voters
        .iter()
        .filter_map(Voter::free_at)
        .min()
        .expect("every voter granted");

    assert!(
        held.lease().fence() < earliest_free,
        "the holder must stop writing strictly before any voter may grant again"
    );
    assert_eq!(held.lease().expiry(), earliest_free);
    assert_eq!(
        held.lease().fence(),
        earliest_free
            .checked_sub(LEASE_GUARD)
            .expect("representable")
    );
}

#[test]
fn a_round_that_dragged_past_the_window_hands_back_one_already_shut() {
    let opened = base();
    let mut voter = settled(opened);
    let mut round = Round::opened_at(Epoch::new(1), A, 1, opened);

    let answered = after(opened, LEASE_TTL.saturating_sub(tenths(1)));
    let vote = voter.asked(&round.ballot(), answered, LEVEL, LEVEL);
    let held = round.counts(ONE, vote).expect("one of one carried it");

    assert!(
        held.lease().fenced(answered),
        "a lease dated from before the asking is already spent when the round was slower than the window"
    );
}

#[test]
fn one_voter_answering_twice_does_not_carry_a_round() {
    let now = base();
    let mut round = Round::opened_at(Epoch::new(1), A, 3, now);
    assert_eq!(round.counts(ONE, Vote::Granted { hold: LEASE_TTL }), None);
    assert_eq!(
        round.counts(ONE, Vote::Granted { hold: LEASE_TTL }),
        None,
        "a majority is a majority of members, not of answers"
    );
    assert!(
        round
            .counts(TWO, Vote::Granted { hold: LEASE_TTL })
            .is_some()
    );
}
