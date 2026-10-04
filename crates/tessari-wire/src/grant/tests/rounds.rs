use super::*;

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
