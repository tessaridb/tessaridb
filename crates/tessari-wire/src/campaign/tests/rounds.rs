use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_round_reports_the_highest_epoch_a_voter_said_it_had_granted() {
    // ADR-0066's learning half, against a real refusal over the wire rather
    // than a constructed one. The peer has already granted epoch 40 to
    // somebody else, so it refuses this ballot and says so — and that number
    // is what stops a candidate climbing one epoch per round towards a
    // cluster it is far behind.
    let authority = Authority::new();
    let voter = [64_u8; NODE_ID_LEN];
    let now = Instant::now();
    let mut spent = Voter::started_at(
        now.checked_sub(LEASE_TTL.saturating_mul(2))
            .expect("this machine has been up for twenty seconds"),
    );
    let elsewhere = Ballot {
        epoch: Epoch::new(40),
        candidate: [200_u8; NODE_ID_LEN],
        range: tessari_types::Reach::Store,
    };
    assert_eq!(
        spent.asked(&elsewhere, now, LEVEL, LEVEL),
        Vote::Granted { hold: LEASE_TTL },
        "the voter has to have granted 40 for the refusal below to name it"
    );
    let (address, answering) = voting(&authority, voter, spent);

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let peers = [(voter, address)];
    let standing = standing(&mine, &said, &peers);

    assert_eq!(
        standing
            .renew(
                &mine_voting(now),
                leaving(Duration::ZERO, now),
                Epoch::new(2),
                now
            )
            .await,
        Stood::Lost {
            granted: Epoch::new(40)
        },
        "the round threw away the one number the refusal was carrying"
    );
    drop(answering.join().expect("the door's thread"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leader_with_margin_left_asks_nobody() {
    // The door would grant — that is what makes this a test of the ordering
    // rather than of the arithmetic. If the canvass ran and its answer was
    // discarded, the epoch would be spent at the voter all the same, and a
    // node that did this on every tick would exhaust the willingness of the
    // very members it needs when its fence finally approaches.
    let authority = Authority::new();
    let voter = [50_u8; NODE_ID_LEN];
    let (address, answering) = voting(&authority, voter, settled());

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let peers = [(voter, address)];
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    let held = leaving(Duration::from_secs(5), now);
    assert_eq!(
        standing
            .renew(&mine_voting(now), held, Epoch::new(2), now)
            .await,
        Stood::NotDue,
        "five seconds of margin against a tenth-second round"
    );

    // The proof that nobody was asked is the voter's own memory: the epoch
    // a canvass would have burnt is still there to be granted.
    let (_, vote) = crate::link::tests::call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        voter,
        &hello(THERE),
        Ask::Ballot(&Ballot {
            epoch: Epoch::new(2),
            candidate: THERE,
            range: tessari_types::Reach::Store,
        }),
    )
    .expect("the door is still up, having been asked nothing");
    assert_eq!(
        vote,
        Answered::Voted(Vote::Granted { hold: LEASE_TTL }),
        "the epoch was never spent, so it is still grantable"
    );
    drop(answering.join().expect("the door's thread"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leader_stands_early_enough_to_lose_a_round_and_still_renew() {
    // A round and a half of margin. One round time still fits, so a cadence
    // that subtracted only one would sit still here and stand at the last
    // moment that can possibly work — leaving nothing for a round that is
    // refused, lost or slow. Two round times is what makes the retry
    // reachable, and this is the case that tells them apart.
    let authority = Authority::new();
    let voter = [51_u8; NODE_ID_LEN];
    let (address, answering) = voting(&authority, voter, settled());

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let peers = [(voter, address)];
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    let held = leaving(
        ROUND
            .checked_add(Duration::from_millis(50))
            .expect("representable"),
        now,
    );
    let won = standing
        .renew(&mine_voting(now), held, Epoch::new(2), now)
        .await
        .won()
        .expect("inside two round times, a leader stands");
    assert_eq!(won.epoch, Epoch::new(2));
    drop(answering.join().expect("the door's thread"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leader_at_its_fence_wins_the_next_epoch() {
    let authority = Authority::new();
    let voters = [
        [52_u8; NODE_ID_LEN],
        [53_u8; NODE_ID_LEN],
        [54_u8; NODE_ID_LEN],
    ];
    let doors: Vec<_> = voters
        .into_iter()
        .map(|id| (id, voting(&authority, id, settled())))
        .collect();
    let peers: Vec<_> = doors
        .iter()
        .map(|(id, (address, _))| (*id, *address))
        .collect();

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    let won = standing
        .renew(
            &mine_voting(now),
            leaving(Duration::ZERO, now),
            Epoch::new(7),
            now,
        )
        .await
        .won()
        .expect("a majority of four — this node and two of its three peers");
    let answered = Instant::now();

    assert_eq!(won.epoch, Epoch::new(7));
    // Dated from when the round opened, not from when the majority came
    // back. The gap between the two is real — three TLS handshakes happened
    // in it — and it is charged to this node's own window rather than to the
    // voters', which is the rule the seam exists to carry.
    assert_eq!(won.from, now, "dated from the instant the round opened");
    assert!(answered > now, "and the collection delay was not nothing");

    // The membership is four — three peers and this node — so a majority is
    // three. Until G053 SG2b the round ended at the second door and the
    // third was never asked; now every door is asked at once, because a
    // renewal is the only evidence a voter has that its leader lives. Each
    // door answers exactly one connection, so every one of them having
    // finished is the proof that every one of them was asked.
    for (_, (_, door)) in doors {
        drop(door.join().expect("the door's thread"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_is_gone_does_not_cost_the_round_its_majority() {
    let authority = Authority::new();
    let gone = [55_u8; NODE_ID_LEN];
    // A real address with nothing behind it: the door is opened to learn
    // where it would have been, then closed. Placed FIRST in the peer set,
    // because a canvass that aborted on the error would still reach a
    // majority if the unreachable member came last.
    let absent = {
        let door = crate::link::tests::bind_with(
            "127.0.0.1:0",
            authority.issue(gone, Purpose::Peer),
            &authority.der(),
        )
        .expect("a door on loopback");
        door.address().expect("its address")
    };

    let voters = [[56_u8; NODE_ID_LEN], [57_u8; NODE_ID_LEN]];
    let doors: Vec<_> = voters
        .into_iter()
        .map(|id| (id, voting(&authority, id, settled())))
        .collect();
    let mut peers = vec![(gone, absent)];
    peers.extend(doors.iter().map(|(id, (address, _))| (*id, *address)));

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    let won = standing
        .renew(
            &mine_voting(now),
            leaving(Duration::ZERO, now),
            Epoch::new(3),
            now,
        )
        .await
        .won()
        .expect("this node and the two that answered carry a membership of four");
    assert_eq!(won.epoch, Epoch::new(3));

    for (_, (_, door)) in doors {
        drop(door.join().expect("the door's thread"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cluster_of_three_carries_a_round_with_one_member_down() {
    // The arithmetic this wave exists for. The membership is three — this
    // node and the two peers an operator declared — so a majority is two:
    // this node's own vote and one peer's. Counting only the peers would
    // make it two OF TWO, and a cluster of three that cannot survive a
    // single loss has no majority in the sense a majority is for. That is
    // not an inefficiency, it is the failover in S7.1 being impossible by
    // arithmetic rather than by any missing mechanism.
    let authority = Authority::new();

    // A real address with nothing behind it, placed FIRST so a round that
    // gave up on the error would not reach the live member either.
    let gone = [60_u8; NODE_ID_LEN];
    let absent = {
        let door = crate::link::tests::bind_with(
            "127.0.0.1:0",
            authority.issue(gone, Purpose::Peer),
            &authority.der(),
        )
        .expect("a door on loopback");
        door.address().expect("its address")
    };

    let alive = [61_u8; NODE_ID_LEN];
    let (address, answering) = voting(&authority, alive, settled());

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let peers = [(gone, absent), (alive, address)];
    let standing = standing(&mine, &said, &peers);

    let now = Instant::now();
    let won = standing
        .renew(
            &mine_voting(now),
            leaving(Duration::ZERO, now),
            Epoch::new(9),
            now,
        )
        .await
        .won()
        .expect("this node and the one peer that answered are two of three");
    assert_eq!(won.epoch, Epoch::new(9));
    drop(answering.join().expect("the door's thread"));
}
