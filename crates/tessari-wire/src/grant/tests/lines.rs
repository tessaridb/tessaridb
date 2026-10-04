use super::*;

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
