use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_reaches_every_voter_and_not_only_a_majority() {
    // G053 SG2b. A voter that hears no renewal has no evidence its leader is
    // alive once the lease is under a second, so it stands — and its ballot
    // raises every voter's epoch past the incumbent's. A canvass that
    // stopped at a majority left one voter in three hearing nothing at all.
    let authority = Authority::new();
    let ids = [
        [70_u8; NODE_ID_LEN],
        [71_u8; NODE_ID_LEN],
        [72_u8; NODE_ID_LEN],
    ];
    let doors: Vec<_> = ids
        .iter()
        .map(|id| (*id, kept_voting(&authority, *id)))
        .collect();
    let peers: Vec<_> = doors
        .iter()
        .map(|(id, (address, _, _))| (*id, *address))
        .collect();

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let standing = standing(&mine, &said, &peers);
    let now = Instant::now();
    let won = standing
        .renew(
            &mine_voting(now),
            leaving(Duration::ZERO, now),
            Epoch::new(5),
            now,
        )
        .await
        .won()
        .expect("four members, all granting");
    assert_eq!(won.epoch, Epoch::new(5));

    for (id, (address, deciding, answering)) in doors {
        let decided = deciding.decided();
        if decided.is_none() {
            // Never asked, so its door is still waiting: knock to release it.
            drop(std::net::TcpStream::connect(address));
        }
        answering.join().expect("the door's thread");
        assert_eq!(
            decided,
            Some(Epoch::new(5)),
            "voter {} heard no renewal, so it has no evidence its leader lives",
            id[0]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_never_answers_costs_the_round_at_most_its_deadline() {
    // A member whose host accepts the connection and then says nothing —
    // a hung process, a half-open link. The dial used the greeting's ten
    // seconds, so one such peer placed first held the whole canvass past
    // every lease this build ships (G053 SG2b).
    let authority = Authority::new();
    let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let quiet = silent.local_addr().expect("its address");
    let holding = std::thread::spawn(move || silent.accept().map(|(socket, _)| socket));

    let alive = [81_u8; NODE_ID_LEN];
    let (address, deciding, answering) = kept_voting(&authority, alive);
    let peers = [([80_u8; NODE_ID_LEN], quiet), (alive, address)];

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let standing = standing(&mine, &said, &peers);
    let now = Instant::now();
    let won = standing
        .renew(
            &mine_voting(now),
            leaving(Duration::ZERO, now),
            Epoch::new(6),
            now,
        )
        .await
        .won();
    let took = now.elapsed();
    drop(holding.join());
    answering.join().expect("the door's thread");

    assert!(
        took < ROUND.saturating_mul(5),
        "a silent member held the round for {took:?} against a {ROUND:?} deadline"
    );
    assert_eq!(won.map(|held| held.epoch), Some(Epoch::new(6)));
    assert_eq!(deciding.decided(), Some(Epoch::new(6)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_voted_for_itself_refuses_that_epoch_to_a_rival() {
    // The reason the self-vote goes through the node's own memory rather
    // than being added to a tally. A vote counted but not recorded would
    // leave this node free to grant the same epoch to somebody else moments
    // later — two candidates holding one epoch, each with an honest
    // majority, and nothing anywhere in an error state.
    let authority = Authority::new();
    let voter = [62_u8; NODE_ID_LEN];
    let (address, answering) = voting(&authority, voter, settled());

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let peers = [(voter, address)];
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    let ours = mine_voting(now);
    let won = standing
        .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(11), now)
        .await
        .won()
        .expect("this node and its one peer are two of two");
    assert_eq!(won.epoch, Epoch::new(11));

    // Asked exactly as a peer would ask, on the connection the door serves.
    let rival = [63_u8; NODE_ID_LEN];
    let vote = ours.asked(
        &Ballot {
            epoch: Epoch::new(11),
            candidate: rival,
            range: tessari_types::Reach::Store,
        },
        Instant::now(),
        LEVEL,
        LEVEL,
    );
    assert!(
        matches!(
            vote,
            Vote::Refused(Refused::EpochAlreadyDecided { granted }) if granted == Epoch::new(11)
        ),
        "a node granted one epoch to two candidates: {vote:?}"
    );
    assert_eq!(
        ours.decided(),
        Some(Epoch::new(11)),
        "the node's own ballot left no trace in its own memory"
    );
    drop(answering.join().expect("the door's thread"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_will_not_vote_for_itself_does_not_count_itself() {
    // A voter that has just started cannot rule out having granted something
    // it has forgotten, so it sits out one TTL. That rule is about the NODE,
    // which means it applies to a ballot the node wrote as surely as to one
    // that arrived on a socket — and a candidate that exempted itself from
    // it would be spending the exact safety the restart rule buys.
    //
    // One peer, so the membership is two and a majority is both. The peer
    // grants; this node refuses itself; the round is not carried.
    let authority = Authority::new();
    let voter = [64_u8; NODE_ID_LEN];
    let (address, answering) = voting(&authority, voter, settled());

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let peers = [(voter, address)];
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    assert_eq!(
        standing
            .renew(
                &Deciding::started(),
                leaving(Duration::ZERO, now),
                Epoch::new(13),
                now
            )
            .await,
        Stood::Lost {
            granted: Epoch::ZERO
        },
        "a node just restarted counted a vote it had refused to cast"
    );
    drop(answering.join().expect("the door's thread"));
}
