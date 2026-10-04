use super::*;

#[test]
fn a_ballot_crosses_the_link_and_comes_back_a_vote() {
    let authority = Authority::new();
    let (address, answering) = voting(&authority, HERE, settled());

    let ballot = Ballot {
        epoch: Epoch::new(12),
        candidate: THERE,
        range: tessari_types::Reach::Store,
    };
    let (_, vote) = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Ballot(&ballot),
    )
    .expect("a peer that proved itself may ask");

    assert_eq!(
        voted(&vote),
        Some(Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        })
    );
    let met = answering
        .join()
        .expect("the door's thread")
        .expect("served");
    assert_eq!(
        met.voted,
        Some(Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        }),
        "both ends saw one answer"
    );
}

#[test]
fn a_candidate_whose_log_is_behind_is_refused_at_the_door() {
    // The wiring test for ADR-0063's second half, and it is the half a unit
    // test cannot reach: the rule lives in the voter, but the position it
    // judges has to arrive from the GREETING the candidate proved rather
    // than from the ballot it wrote. A door that passed the ballot's word
    // for it would pass every unit test in `grant` and restrict nothing.
    let authority = Authority::new();
    let (address, answering) = voting(&authority, HERE, settled());

    let (_, vote) = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &falling_behind(THERE),
        Ask::Ballot(&Ballot {
            epoch: Epoch::new(12),
            candidate: THERE,
            range: tessari_types::Reach::Store,
        }),
    )
    .expect("a peer that proved itself may ask");

    assert_eq!(
        voted(&vote),
        Some(Vote::Refused(Refused::LogBehind {
            leadership: LEVEL.leadership,
            tail: LEVEL.tail,
        })),
        "the door judged the position the candidate greeted with"
    );
    let met = answering
        .join()
        .expect("the door's thread")
        .expect("served");
    assert_eq!(met.voted, voted(&vote), "both ends saw one answer");
}

#[test]
fn a_ballot_naming_somebody_else_never_reaches_the_voter() {
    // The hole W228 opens and closes in the same wave. A voter now grants a
    // ballot from the node it is already holding a grant for — so a peer
    // free to write the incumbent's id into its own ballot would collect
    // exactly the grants the liveness rule exists to withhold, and the
    // cluster would have two holders.
    //
    // The credential says THERE and the ballot says HERE. Refused at the
    // door, before the voter is asked anything at all.
    let authority = Authority::new();
    let peers = bind_with(
        "127.0.0.1:0",
        authority.issue(HERE, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");

    let mine = hello(HERE);
    let answering = std::thread::spawn(move || {
        let voter = Deciding::holding(settled());
        let met = peers.greet(|| Ok(mine), &HERE, &voter, &NoLog);
        // The voter is handed back untouched: nothing was decided, which is
        // the half a refusal-shaped answer would not have given.
        (met, voter.decided())
    });

    let _ = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Ballot(&Ballot {
            epoch: Epoch::new(1),
            candidate: HERE,
            range: tessari_types::Reach::Store,
        }),
    );

    let (met, decided) = answering.join().expect("the door's thread");
    assert!(
        matches!(met, Err(Error::NotItsOwnBallot)),
        "expected the door to refuse the ballot outright, got {met:?}"
    );
    assert_eq!(decided, None, "the voter was never asked");
}

#[test]
fn a_refusal_keeps_its_reason_and_its_wait_across_the_wire() {
    let authority = Authority::new();
    let peers = bind_with(
        "127.0.0.1:0",
        authority.issue(HERE, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");

    // A grant this voter is already holding for somebody ELSE, so what
    // crosses the wire is a challenger and not a renewal. W228 made that
    // distinction decide the vote: the same candidate asking again is
    // granted, because re-granting to the holder adds no second holder.
    let mine = hello(HERE);
    let answering = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &Deciding::holding(incumbent()), &NoLog)
    });

    let refused = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &hello(THERE),
        Ask::Ballot(&Ballot {
            epoch: Epoch::new(2),
            candidate: THERE,
            range: tessari_types::Reach::Store,
        }),
    )
    .expect("a peer that proved itself may ask")
    .1;
    let refused = voted(&refused).expect("a vote came back");
    drop(answering.join().expect("the door's thread"));

    // The reason survives, and so does the wait: a candidate told only "no"
    // cannot tell waiting from being wrong.
    match refused {
        Vote::Refused(Refused::EarlierGrantStillAlive { for_the_next }) => {
            assert!(
                for_the_next > Duration::ZERO && for_the_next <= tessari_storage::LEASE_TTL,
                "{for_the_next:?}"
            );
        }
        other => unreachable!("expected a live-grant refusal, got {other:?}"),
    }
}
