use super::round::majority;
use super::{Ballot, Deciding, Leadership, Reached, Refused, Round, Vote, Voter};
use std::time::{Duration, Instant};
use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{LEASE_GUARD, LEASE_TTL};

/// `k` tenths of the lease. These cases were written in whole seconds
/// against a ten-second lease; stated as fractions of it, they keep their
/// meaning whatever the lease is (G053 SG2b).
fn tenths(k: u32) -> Duration {
    LEASE_TTL
        .checked_div(10)
        .expect("a lease divides")
        .saturating_mul(k)
}
use tessari_types::{DatabaseId, Epoch, NamespaceId, Reach, Sequence, ShardId, TableId};

/// A log position both sides of a vote share.
///
/// Every case below is about the lease rules, so the two logs are level and
/// the restriction added in W253 never fires — a test about when a voter may
/// grant should not also be a test about what it is granting to.
const LEVEL: Reached = Reached {
    leadership: Epoch::new(3),
    tail: Sequence::new(9),
};

/// A base far enough ahead that every test can subtract from it without
/// depending on how long this machine has been up.
fn base() -> Instant {
    after(Instant::now(), Duration::from_secs(3600))
}

/// Checked throughout, because the workspace denies loose arithmetic
/// everywhere and a test is not an exception to a rule about overflow.
fn after(at: Instant, by: Duration) -> Instant {
    at.checked_add(by).expect("representable")
}

/// A voter that has been up long enough to have outlived anything it might
/// have granted before a restart.
fn settled(at: Instant) -> Voter {
    Voter::started_at(at.checked_sub(LEASE_TTL).expect("representable"))
}

const A: [u8; NODE_ID_LEN] = [0xA1; NODE_ID_LEN];
const B: [u8; NODE_ID_LEN] = [0xB2; NODE_ID_LEN];
const C: [u8; NODE_ID_LEN] = [0xC3; NODE_ID_LEN];
const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const TWO: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
const THREE: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

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

#[test]
fn a_majority_is_strictly_more_than_half() {
    for (voters, needed) in [(1, 1), (2, 2), (3, 2), (4, 3), (5, 3), (6, 4), (7, 4)] {
        assert_eq!(majority(voters), needed, "majority of {voters}");
    }

    // The even case stated as the failure it prevents: two disjoint halves
    // of a set of four must not each carry a round.
    let now = base();
    let mut ours = Round::opened_at(Epoch::new(1), A, 4, now);
    ours.counts(ONE, Vote::Granted { hold: LEASE_TTL });
    assert_eq!(
        ours.counts(TWO, Vote::Granted { hold: LEASE_TTL }),
        None,
        "half is not enough"
    );
}

#[test]
fn a_round_against_no_voters_can_never_conclude() {
    let now = base();
    let round = Round::opened_at(Epoch::new(1), A, 0, now);
    assert_eq!(round.held(), None);
}

#[test]
fn a_candidate_behind_this_voter_is_refused_and_told_how_far_to_come() {
    // ADR-0063's second half. Widening who may stand without this turns a
    // liveness improvement into a way to lose data: a candidate holding less
    // history wins, leads, and the writes it never received are gone with
    // nothing in an error state.
    let now = base();
    let mut voter = settled(now);
    let ballot = Ballot {
        epoch: Epoch::new(4),
        candidate: A,
        range: tessari_types::Reach::Store,
    };
    let behind = Reached {
        leadership: LEVEL.leadership,
        tail: Sequence::new(LEVEL.tail.get().saturating_sub(1)),
    };

    assert_eq!(
        voter.asked(&ballot, now, LEVEL, behind),
        Vote::Refused(Refused::LogBehind {
            leadership: LEVEL.leadership,
            tail: LEVEL.tail,
        }),
        "the refusal names the VOTER'S position, which is the half the \
             candidate does not already know"
    );
    // And the refusal is about the log rather than about this voter's state:
    // it granted nothing, so the same candidate level with it is granted.
    assert_eq!(voter.decided(), None);
    assert_eq!(
        voter.asked(&ballot, now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
}

#[test]
fn a_candidate_ahead_of_this_voter_is_not_refused_for_being_ahead() {
    // Strictly behind, not merely different. A voter that refused everyone
    // it was not level with would refuse every candidate in a cluster where
    // anything had been written since it last collected — which is every
    // cluster, most of the time.
    let now = base();
    let mut voter = settled(now);
    let ahead = Reached {
        leadership: LEVEL.leadership,
        tail: Sequence::new(LEVEL.tail.get().saturating_add(40)),
    };
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(4),
                candidate: A,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            ahead
        ),
        Vote::Granted { hold: LEASE_TTL }
    );
}

#[test]
fn a_longer_log_under_an_older_leadership_still_loses() {
    // The reason the comparison is a pair and not a number. A node that led
    // an epoch, wrote records no majority ever saw, and fell away holds a
    // HIGHER sequence than the node carrying the history that actually won.
    // Ranking on the sequence alone would hand leadership to the diverged
    // branch and call it the most up-to-date.
    let now = base();
    let mut voter = settled(now);
    let diverged = Reached {
        leadership: Epoch::new(LEVEL.leadership.get().saturating_sub(1)),
        tail: Sequence::new(LEVEL.tail.get().saturating_add(1_000)),
    };
    assert!(diverged.behind(LEVEL), "a lower leadership is behind");
    assert_eq!(
        voter.asked(
            &Ballot {
                epoch: Epoch::new(9),
                candidate: A,
                range: tessari_types::Reach::Store,
            },
            now,
            LEVEL,
            diverged
        ),
        Vote::Refused(Refused::LogBehind {
            leadership: LEVEL.leadership,
            tail: LEVEL.tail,
        })
    );
    // And the other direction, which is what makes the pair an ordering
    // rather than a preference: a shorter log under a newer leadership wins.
    assert!(!LEVEL.behind(diverged));
}

#[test]
fn the_leader_renewing_its_own_epoch_is_never_behind_what_it_wrote() {
    // G057 SG6. The candidate's position comes from the greeting it proved,
    // read before a handshake of several round trips; this voter's is read
    // when the ballot lands. A leader committing all the while has streamed
    // this voter entries past the greeting, so across distance the voter
    // looked ahead of the very leader whose epoch wrote its tail — and
    // refused the renewal until the lease ran out under writes. Everything
    // written under one epoch was written by its one leader, so its renewal
    // holds it by construction; an election is still judged as before.
    let now = base();
    let mut voter = settled(now);
    let epoch = Epoch::new(7);
    let renewal = Ballot {
        epoch,
        candidate: A,
        range: tessari_types::Reach::Store,
    };
    let greeted = Reached {
        leadership: epoch,
        tail: Sequence::new(2),
    };
    assert_eq!(
        voter.asked(&renewal, now, greeted, greeted),
        Vote::Granted { hold: LEASE_TTL }
    );
    let streamed = Reached {
        leadership: epoch,
        tail: Sequence::new(41),
    };
    let later = after(now, tenths(3));
    assert_eq!(
        voter.asked(&renewal, later, streamed, greeted),
        Vote::Granted { hold: LEASE_TTL },
        "the incumbent's renewal of its own epoch"
    );
    // Control: a challenger with the same stale position is still behind.
    let mut other = settled(now);
    assert_eq!(
        other.asked(
            &Ballot {
                epoch: Epoch::new(8),
                candidate: B,
                range: tessari_types::Reach::Store,
            },
            now,
            streamed,
            greeted
        ),
        Vote::Refused(Refused::LogBehind {
            leadership: epoch,
            tail: Sequence::new(41),
        })
    );
}

#[test]
fn two_logs_at_one_position_are_behind_neither() {
    // The self-vote depends on this: a candidate asks its own memory with
    // its own position on both sides, and a rule that refused equality would
    // stop every node voting for itself.
    assert!(!LEVEL.behind(LEVEL));
}

#[test]
fn a_refusal_that_names_a_log_crosses_the_wire() {
    // The reason a refusal carries values at all: *catch up to sequence 9*
    // and *you are re-running a decided epoch* send a candidate to different
    // places, and a wire that kept only the "no" would be less informative
    // than the rule behind it.
    let refused = Vote::Refused(Refused::LogBehind {
        leadership: Epoch::new(6),
        tail: Sequence::new(4_096),
    });
    assert_eq!(
        Vote::decode(&refused.encode()).expect("a vote this build wrote"),
        refused
    );
}

// ---- G032 S3.1 and S3.2: a ballot names its line -------------------------

fn shard(n: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        ShardId::new(n),
    )
}

#[test]
fn a_store_ballot_keeps_its_twenty_four_bytes() {
    // The kill criterion (G032), from `Ballot::encode` as it stood at
    // `a1d0025`: the epoch big-endian, then the candidate.
    let ballot = Ballot {
        epoch: Epoch::new(7),
        candidate: [5; NODE_ID_LEN],
        range: Reach::Store,
    };
    let mut golden = vec![0, 0, 0, 0, 0, 0, 0, 7];
    golden.extend_from_slice(&[5; NODE_ID_LEN]);
    assert_eq!(ballot.encode(), golden);
    assert_eq!(Ballot::decode(&golden).expect("a store ballot"), ballot);
}

#[test]
fn a_range_ballot_round_trips_and_a_cut_range_is_refused() {
    let ballot = Round::opened(Epoch::new(3), [6; NODE_ID_LEN], 3)
        .over(shard(2))
        .ballot();
    assert_eq!(ballot.range, shard(2));
    let body = ballot.encode();
    assert_eq!(Ballot::decode(&body).expect("a range ballot"), ballot);
    for stop in 25..body.len() {
        let cut = body.get(..stop).expect("a prefix");
        assert!(
            Ballot::decode(cut).is_err(),
            "{stop} bytes read as a ballot"
        );
    }
}

fn settled_deciding() -> Deciding {
    Deciding::holding(Voter::started_at(
        base()
            .checked_sub(LEASE_TTL)
            .expect("an hour ahead minus ten seconds"),
    ))
}

fn ballot(epoch: u64, candidate: u8, range: Reach) -> Ballot {
    Ballot {
        epoch: Epoch::new(epoch),
        candidate: [candidate; NODE_ID_LEN],
        range,
    }
}

#[test]
fn a_grant_on_one_line_never_answers_a_ballot_on_another() {
    let deciding = settled_deciding();
    let now = base();
    let vote = |ballot: &Ballot| deciding.asked(ballot, now, LEVEL, LEVEL);
    assert_eq!(
        vote(&ballot(1, 1, shard(1))),
        Vote::Granted { hold: LEASE_TTL }
    );
    // The same line and epoch for somebody else: one epoch, one candidate.
    assert!(matches!(
        vote(&ballot(1, 2, shard(1))),
        Vote::Refused(Refused::EpochAlreadyDecided { .. })
    ));
    // Another line's epoch 1 is another counter, and so is the store's.
    assert_eq!(
        vote(&ballot(1, 2, shard(2))),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(
        vote(&ballot(1, 2, Reach::Store)),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(
        deciding.granted_elsewhere_on(shard(1), [2; NODE_ID_LEN]),
        Some(now)
    );
    assert_eq!(
        deciding.granted_elsewhere_on(shard(1), [1; NODE_ID_LEN]),
        None
    );
    assert_eq!(
        deciding.granted_elsewhere_on(shard(3), [2; NODE_ID_LEN]),
        None
    );
}

#[test]
fn a_grant_to_a_new_store_epoch_is_announced_and_a_renewal_is_not() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let deciding = settled_deciding();
    let announced = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&announced);
    assert!(deciding.when_granted_anew(Box::new(move || {
        counting.fetch_add(1, Ordering::Relaxed);
    })));
    let now = base();
    let candidate = [1; NODE_ID_LEN];

    assert_eq!(
        deciding.asked(&store_ballot(1, candidate), now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(announced.load(Ordering::Relaxed), 1);
    // The incumbent renewing its epoch is the same leadership.
    assert_eq!(
        deciding.asked(&store_ballot(1, candidate), now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(announced.load(Ordering::Relaxed), 1);
    // A placed range's line is not the store's leadership.
    let _ = deciding.asked(&ballot(1, 2, shard(1)), now, LEVEL, LEVEL);
    assert_eq!(announced.load(Ordering::Relaxed), 1);
    // Free again, a later epoch is a new leadership.
    let later = now
        .checked_add(LEASE_TTL.saturating_mul(2))
        .expect("in range");
    assert_eq!(
        deciding.asked(&store_ballot(2, candidate), later, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL }
    );
    assert_eq!(announced.load(Ordering::Relaxed), 2);
}

#[test]
fn every_line_starts_when_the_process_did() {
    // A restarted voter cannot remember a grant on ANY line, so a line it
    // has never been asked about is as young as the process.
    let started = base();
    let deciding = Deciding::holding(Voter::started_at(started));
    let vote = deciding.asked(&ballot(1, 1, shard(1)), started, LEVEL, LEVEL);
    assert!(
        matches!(vote, Vote::Refused(Refused::TooSoonAfterStarting { .. })),
        "{vote:?}"
    );
}

/// A ballot for `candidate` at `epoch` on the store's line.
fn store_ballot(epoch: u64, candidate: [u8; NODE_ID_LEN]) -> Ballot {
    Ballot {
        epoch: Epoch::new(epoch),
        candidate,
        range: Reach::Store,
    }
}

#[test]
fn a_voter_holds_its_grant_for_the_lease_its_policy_states() {
    // G053 SG2c (Q-878). A policy that lengthens the lease lengthens what a
    // voter promises, or a holder writing under the long lease would meet a
    // voter that had already freed itself on the build's short one.
    let now = base();
    let hold = LEASE_TTL.saturating_mul(4);
    let mut voter =
        Voter::started_at(now.checked_sub(hold).expect("representable")).holding_for(hold);
    assert_eq!(
        voter.asked(&store_ballot(1, A), now, LEVEL, LEVEL),
        Vote::Granted { hold },
        "a grant states how long its voter will hold it"
    );
    let past_the_built_in_lease = after(now, LEASE_TTL.saturating_add(tenths(1)));
    let vote = voter.asked(&store_ballot(2, B), past_the_built_in_lease, LEVEL, LEVEL);
    assert!(
        matches!(vote, Vote::Refused(Refused::EarlierGrantStillAlive { .. })),
        "a voter freed itself on the build's lease while its policy's still ran: {vote:?}"
    );
    assert_eq!(voter.free_at(), Some(after(now, hold)));
}

#[test]
fn a_restarted_voter_sits_out_the_longer_of_its_policy_and_the_build() {
    // A restarted voter cannot remember what it granted, and what it granted
    // was held for the policy it ran under — which the store still carries.
    let started = base();
    let hold = LEASE_TTL.saturating_mul(4);
    let mut voter = Voter::started_at(started).holding_for(hold);
    let vote = voter.asked(
        &store_ballot(1, A),
        after(started, LEASE_TTL.saturating_add(tenths(1))),
        LEVEL,
        LEVEL,
    );
    assert!(
        matches!(vote, Vote::Refused(Refused::TooSoonAfterStarting { .. })),
        "{vote:?}"
    );
    let mut short = Voter::started_at(started).holding_for(tenths(2));
    let vote = short.asked(&store_ballot(1, A), after(started, tenths(5)), LEVEL, LEVEL);
    assert!(
        matches!(vote, Vote::Refused(Refused::TooSoonAfterStarting { .. })),
        "a short policy shortened the restart window below the build's lease: {vote:?}"
    );
}

#[test]
fn a_lease_is_no_longer_than_the_shortest_hold_that_carried_it() {
    // The holder stops before ANY voter that granted it is free, whichever
    // policy each had installed when it answered — and before its own.
    let opened = base();
    let mut round =
        Round::opened_at(Epoch::new(3), A, 3, opened).leasing(LEASE_TTL.saturating_mul(4));
    assert_eq!(
        round.counts(
            ONE,
            Vote::Granted {
                hold: LEASE_TTL.saturating_mul(4)
            }
        ),
        None
    );
    let held = round
        .counts(
            TWO,
            Vote::Granted {
                hold: LEASE_TTL.saturating_mul(2),
            },
        )
        .expect("two of three");
    assert_eq!(
        held.lease().expiry(),
        after(opened, LEASE_TTL.saturating_mul(2)),
        "the lease outlived a voter's hold"
    );

    let mut modest = Round::opened_at(Epoch::new(4), A, 1, opened).leasing(LEASE_TTL);
    let held = modest
        .counts(
            ONE,
            Vote::Granted {
                hold: LEASE_TTL.saturating_mul(4),
            },
        )
        .expect("one of one");
    assert_eq!(
        held.lease().expiry(),
        after(opened, LEASE_TTL),
        "a voter's longer hold lengthened the candidate's own lease"
    );
}

#[test]
fn a_grant_states_its_hold_on_the_wire_and_a_bare_one_is_the_builds() {
    let hold = Duration::from_millis(3_250);
    let granted = Vote::Granted { hold };
    assert_eq!(Vote::decode(&granted.encode()).ok(), Some(granted));
    // What a build from before the field sends: the tag alone. It held its
    // grant for its own lease, and the shortest this build assumes is its own.
    assert_eq!(
        Vote::decode(&[0]).ok(),
        Some(Vote::Granted { hold: LEASE_TTL })
    );
}
