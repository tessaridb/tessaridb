use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_round_the_peers_carried_is_recorded_in_the_winners_own_memory() {
    // G053 SG2d, the kill test's lease lapse. This node granted a rival an
    // epoch moments ago, so its own voter refuses it the next one for a
    // whole TTL — and the two peers carry the round without it. A win its
    // own memory does not hold reads, at the standing gate, as a grant to
    // somebody else: the leader silenced itself for the rest of its lease
    // and never renewed. And past that TTL the same memory would grant the
    // NEXT epoch to a challenger while this node still writes under this one.
    let authority = Authority::new();
    let ids = [[73_u8; NODE_ID_LEN], [74_u8; NODE_ID_LEN]];
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
    let ours = mine_voting(now);
    let rival = [75_u8; NODE_ID_LEN];
    let rivals_grant = now
        .checked_sub(LEASE_TTL / 2)
        .expect("this machine has been up for a second");
    let earlier = ours.asked(
        &Ballot {
            epoch: Epoch::new(6),
            candidate: rival,
            range: tessari_types::Reach::Store,
        },
        rivals_grant,
        LEVEL,
        LEVEL,
    );
    assert_eq!(
        earlier,
        Vote::Granted { hold: LEASE_TTL },
        "the rival's grant this test starts from"
    );

    let won = standing
        .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(7), now)
        .await
        .won()
        .expect("both peers granted, which is two of three without this node");
    assert_eq!(won.epoch, Epoch::new(7));
    assert_eq!(
        ours.granted_elsewhere_at(THERE),
        None,
        "a leader reads its own win as a live grant to somebody else"
    );
    assert_eq!(ours.decided(), Some(Epoch::new(7)));

    let challenger = [76_u8; NODE_ID_LEN];
    // The rival's hold is over and the win's is not: only the win can
    // refuse this ballot now.
    let past_the_rivals_hold = rivals_grant.checked_add(LEASE_TTL).expect("representable");
    let vote = ours.asked(
        &Ballot {
            epoch: Epoch::new(8),
            candidate: challenger,
            range: tessari_types::Reach::Store,
        },
        past_the_rivals_hold,
        LEVEL,
        LEVEL,
    );
    assert!(
        matches!(vote, Vote::Refused(Refused::EarlierGrantStillAlive { .. })),
        "a sitting leader's own voter granted a challenger the next epoch: {vote:?}"
    );
    for (_, (_, _, answering)) in doors {
        answering.join().expect("the door's thread");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_carried_at_the_epoch_this_node_granted_a_rival_is_recorded() {
    // The second shape of the kill test's lapse. Three candidates stand at
    // one epoch; this node grants a rival that epoch at its door, then both
    // peers carry the same epoch for THIS node. Each voter grants an epoch
    // once, so the rival's bid at it lost — and a memory that kept naming
    // the rival silenced the winner exactly as a lower epoch did.
    let authority = Authority::new();
    let ids = [[77_u8; NODE_ID_LEN], [78_u8; NODE_ID_LEN]];
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
    let ours = mine_voting(now);
    let rival = [79_u8; NODE_ID_LEN];
    let earlier = ours.asked(
        &Ballot {
            epoch: Epoch::new(6),
            candidate: rival,
            range: tessari_types::Reach::Store,
        },
        now,
        LEVEL,
        LEVEL,
    );
    assert_eq!(
        earlier,
        Vote::Granted { hold: LEASE_TTL },
        "the rival's grant this test starts from"
    );

    let won = standing
        .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(6), now)
        .await
        .won()
        .expect("both peers granted, which is two of three without this node");
    assert_eq!(won.epoch, Epoch::new(6));
    assert_eq!(
        ours.granted_elsewhere_at(THERE),
        None,
        "a leader reads the rival's lost bid at its own epoch as a live grant"
    );
    for (_, (_, _, answering)) in doors {
        answering.join().expect("the door's thread");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_carried_below_an_epoch_this_node_already_granted_is_not_taken() {
    // The third shape, and the one that is not only liveness. While this
    // node canvassed for epoch 7, its door granted a rival epoch 8 — and
    // with it this node's log position as it stood. Leading at 7 after that
    // would append records that rival's line was promised it did not need,
    // acknowledged and then lost if the rival wins. Raft's rule: a node that
    // voted in a higher term is a follower in it, and an election for the
    // lower term is over whatever its replies say.
    let authority = Authority::new();
    let ids = [[80_u8; NODE_ID_LEN], [81_u8; NODE_ID_LEN]];
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
    let ours = mine_voting(now);
    let rival = [82_u8; NODE_ID_LEN];
    let promised = ours.asked(
        &Ballot {
            epoch: Epoch::new(8),
            candidate: rival,
            range: tessari_types::Reach::Store,
        },
        now,
        LEVEL,
        LEVEL,
    );
    assert_eq!(
        promised,
        Vote::Granted { hold: LEASE_TTL },
        "the rival's grant this test starts from"
    );

    assert_eq!(
        standing
            .renew(&ours, leaving(Duration::ZERO, now), Epoch::new(7), now)
            .await,
        Stood::Lost {
            granted: Epoch::new(8)
        },
        "a node took leadership of an epoch below one it had already granted a rival"
    );
    assert_eq!(
        ours.decided(),
        Some(Epoch::new(8)),
        "the grant to the rival was overwritten by a round it outranks"
    );
    for (_, (_, _, answering)) in doors {
        answering.join().expect("the door's thread");
    }
}
