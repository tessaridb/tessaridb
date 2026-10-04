use super::*;

#[test]
fn a_candidate_that_reaches_a_majority_holds_the_epoch() {
    let authority = Authority::new();
    let doors: Vec<_> = [
        [10_u8; NODE_ID_LEN],
        [11_u8; NODE_ID_LEN],
        [12_u8; NODE_ID_LEN],
    ]
    .into_iter()
    .map(|id| (id, voting(&authority, id, settled())))
    .collect();

    let mut round = Round::opened(Epoch::new(5), THERE, doors.len());
    let mut held = None;
    for (id, (address, _)) in &doors {
        let (_, vote) = call_with(
            *address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            *id,
            &hello(THERE),
            Ask::Ballot(&round.ballot()),
        )
        .expect("every door is up");
        held = round.counts(*id, voted(&vote).expect("a door that was asked answers"));
    }

    for (_, (_, answering)) in doors {
        drop(answering.join().expect("the door's thread"));
    }
    let held = held.expect("three of three carried it");
    assert_eq!(held.epoch, Epoch::new(5));
}

#[test]
fn a_challenger_a_majority_refuses_holds_nothing() {
    let authority = Authority::new();
    // Every door is up and every door says no, because a leader already
    // holds the epoch before this one. This is the ordinary failure — far
    // more common than a partition — and it is the one where a candidate
    // that counted answers rather than grants would elect itself.
    let doors: Vec<_> = [
        [40_u8; NODE_ID_LEN],
        [41_u8; NODE_ID_LEN],
        [42_u8; NODE_ID_LEN],
    ]
    .into_iter()
    .map(|id| (id, voting(&authority, id, incumbent())))
    .collect();

    let mut round = Round::opened(Epoch::new(2), THERE, doors.len());
    for (id, (address, _)) in &doors {
        let (_, vote) = call_with(
            *address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            *id,
            &hello(THERE),
            Ask::Ballot(&round.ballot()),
        )
        .expect("every door is up and answering");
        let vote = voted(&vote).expect("a door that was asked answers");
        assert!(
            matches!(vote, Vote::Refused(Refused::EarlierGrantStillAlive { .. })),
            "{vote:?}"
        );
        assert_eq!(round.counts(*id, vote), None, "a refusal is not a grant");
    }

    for (_, (_, answering)) in doors {
        drop(answering.join().expect("the door's thread"));
    }
    assert_eq!(round.held(), None, "three noes are not a majority of yeses");
}

#[test]
fn a_candidate_partitioned_from_the_majority_holds_nothing() {
    let authority = Authority::new();
    // Three voting members configured; one door is up. The other two are
    // not refusing — they are gone, which is what a partition looks like
    // from here and is the only version of it worth testing.
    let alive = [20_u8; NODE_ID_LEN];
    let (address, answering) = voting(&authority, alive, settled());
    let unreachable = bind_with(
        "127.0.0.1:0",
        authority.issue([21_u8; NODE_ID_LEN], Purpose::Peer),
        &authority.der(),
    )
    .expect("a door, briefly");
    let vanished = unreachable.address().expect("its address");
    drop(unreachable);

    let mut round = Round::opened(Epoch::new(9), THERE, 3);
    let (_, vote) = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        alive,
        &hello(THERE),
        Ask::Ballot(&round.ballot()),
    )
    .expect("the one door that is up answers");
    assert_eq!(
        round.counts(alive, voted(&vote).expect("it answered")),
        None,
        "one of three is not a majority"
    );

    let reached = call_with(
        vanished,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        [21_u8; NODE_ID_LEN],
        &hello(THERE),
        Ask::Ballot(&round.ballot()),
    );
    assert!(reached.is_err(), "a door that is gone answers nothing");

    drop(answering.join().expect("the door's thread"));
    assert_eq!(round.held(), None, "the round never concluded");
}

#[test]
fn a_leader_that_could_not_renew_refuses_writes_before_its_lease_expires() {
    let authority = Authority::new();
    let store = tessaridb::Db::in_memory().expect("a store");
    store
        .session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE COLLECTION users;",
        )
        .expect("a place to write");

    // A majority grants, and the node takes the lease that grant entitles it
    // to. The span is short so the fence is reachable inside a test; the
    // arithmetic it runs is the same one the shipped lease runs.
    let voters = [
        [30_u8; NODE_ID_LEN],
        [31_u8; NODE_ID_LEN],
        [32_u8; NODE_ID_LEN],
    ];
    let doors: Vec<_> = voters
        .into_iter()
        .map(|id| (id, voting(&authority, id, settled())))
        .collect();

    let mut round = Round::opened(Epoch::new(1), THERE, voters.len());
    let mut held = None;
    for (id, (address, _)) in &doors {
        let (_, vote) = call_with(
            *address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            *id,
            &hello(THERE),
            Ask::Ballot(&round.ballot()),
        )
        .expect("every door is up");
        held = round.counts(*id, voted(&vote).expect("it answered"));
    }
    let held = held.expect("three of three carried it");

    // The majority goes away — every door joined and dropped, so the
    // addresses are real and nothing is listening on them. That is the
    // partition, and it is a partition of the whole majority rather than of
    // one convenient peer.
    let addresses: Vec<_> = doors
        .into_iter()
        .map(|(id, (address, answering))| {
            drop(answering.join().expect("the door's thread"));
            (id, address)
        })
        .collect();

    let ttl = tessari_storage::LEASE_GUARD
        .checked_add(Duration::from_millis(400))
        .expect("representable");
    let taken = std::time::Instant::now();
    store.hold_lease(ttl);
    assert_eq!(held.epoch, Epoch::new(1));
    store
        .session()
        .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:1 = { name: 'ada' };")
        .expect("a leader inside its window writes");

    // Now the partition: the voter is gone, so the renewal round cannot
    // reach anyone, let alone a majority, and nothing renews.
    let renewal = Round::opened(Epoch::new(2), THERE, addresses.len());
    for (id, address) in &addresses {
        let reached = call_with(
            *address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            *id,
            &hello(THERE),
            Ask::Ballot(&renewal.ballot()),
        );
        assert!(reached.is_err(), "the majority is unreachable");
    }
    assert_eq!(renewal.held(), None, "so the renewal grants nothing");

    // Past the fence, which is `ttl - GUARD` = 400 ms, and short of the
    // expiry by half the guard. That gap is the whole point: the holder
    // stops writing while the cluster still may not reassign. Aimed at the
    // middle of the guard from the instant the lease was taken, because the
    // guard is 150 ms and a fixed sleep after the writes above would spend
    // part of it on them (G053 SG2b).
    let middle = Duration::from_millis(400)
        .saturating_add(tessari_storage::LEASE_GUARD.checked_div(2).expect("halves"));
    std::thread::sleep(middle.saturating_sub(taken.elapsed()));
    let refused = store
        .session()
        .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:2 = { name: 'grace' };")
        .expect_err("a leader that could not renew stops writing");
    let elapsed = taken.elapsed();

    // The criterion's own sentence, measured on monotonic elapsed time: the
    // refusal happened, and it happened strictly before the lease expired.
    let fence = ttl
        .checked_sub(tessari_storage::LEASE_GUARD)
        .expect("a ttl longer than the guard");
    assert!(elapsed >= fence, "refused before the fence: {elapsed:?}");
    assert!(
        elapsed < ttl,
        "refused after the expiry, not before it: {elapsed:?}"
    );
    let said = refused.to_string();
    assert!(said.contains("lease"), "{said}");
}

#[test]
fn the_lease_a_round_won_is_the_lease_the_node_holds() {
    // The seam between a round and the fence, asserted without a socket
    // because the socket is not what is in question. A granted lease is
    // dated from the instant its round OPENED, and installing it has to
    // carry that instant: a span cannot, because by the time one arrives the
    // collection delay has already been spent, and restarting the clock here
    // would spend it a second time out of the VOTERS' window instead of this
    // node's — which is the split-brain the dating rule exists to prevent,
    // reached through the seam rather than through the rule.
    let store = tessaridb::Db::in_memory().expect("a store");
    store
        .session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE COLLECTION users;",
        )
        .expect("a place to write");
    let voter = [40_u8; NODE_ID_LEN];

    // A round that opened now and was carried at once.
    let mut prompt = Round::opened(Epoch::new(1), THERE, 1);
    let won = prompt
        .counts(
            voter,
            Vote::Granted {
                hold: tessari_storage::LEASE_TTL,
            },
        )
        .expect("one of one carries it");
    store.hold(won.epoch, won.lease());
    store
        .session()
        .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:1 = { name: 'ada' };")
        .expect("a round that cost nothing hands over the whole window");

    // The same round, opened a whole TTL ago. Nothing else differs.
    let opened = std::time::Instant::now()
        .checked_sub(tessari_storage::LEASE_TTL)
        .expect("representable");
    let mut slow = Round::opened_at(Epoch::new(2), THERE, 1, opened);
    let won = slow
        .counts(
            voter,
            Vote::Granted {
                hold: tessari_storage::LEASE_TTL,
            },
        )
        .expect("one of one carries it");
    store.hold(won.epoch, won.lease());
    let refused = store
        .session()
        .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:2 = { name: 'grace' };")
        .expect_err("a round that took the whole TTL hands over no window at all");
    let said = refused.to_string();
    assert!(said.contains("lease"), "{said}");
}
