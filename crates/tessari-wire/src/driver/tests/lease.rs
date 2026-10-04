use super::*;

#[test]
fn a_peer_running_a_newer_failover_policy_holds_this_node_back() {
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.1:9000", running(4, 0))]);
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            Some(stamp(3, 9)),
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        Some(stamp(4, 0)),
        "a candidate timing itself by a policy the cluster has replaced was              not held back, which is the disagreement the policy row exists to              remove arriving at the moment it decides an outcome"
    );
}

#[test]
fn an_equal_or_older_policy_holds_nobody_back() {
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let now = Instant::now();
    for (peer, mine, why) in [
        (
            running(3, 9),
            stamp(3, 9),
            "an equal pair is the ordinary state of an agreeing cluster and                  must never stop an election",
        ),
        (
            running(3, 8),
            stamp(3, 9),
            "a lower version is a peer that is behind, which is the ordinary                  state of a follower and not a reason to refuse",
        ),
        (
            running(2, 99),
            stamp(3, 0),
            "a superseded leadership does not win on version — this is the                  partitioned ex-leader reconnecting, and letting it silence a                  candidate would hand it the outcome it lost",
        ),
    ] {
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &greeted(&[("10.0.0.1:9000", peer)]),
                Some(mine),
                now,
                tessari_storage::LEASE_TTL
            ),
            None,
            "{why}"
        );
    }
}

#[test]
fn a_node_that_holds_no_policy_at_all_is_behind_one_that_does() {
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.1:9000", running(1, 0))]);
    // The first policy a cluster ever sets is the case this covers. Treating
    // *no policy* as unbeatable would make that first one the single policy
    // nothing could ever act on.
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        Some(stamp(1, 0))
    );
    // And the other direction: a peer that says nothing supersedes nothing,
    // so a build from before the field cannot silence the cluster it joins.
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &greeted(&[("10.0.0.1:9000", said())]),
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        None,
        "a greeting carrying no policy silenced a candidate, which would              make a rolling upgrade an outage"
    );
}

#[test]
fn a_greeting_older_than_the_lease_cannot_hold_a_candidate_back() {
    // This is what makes the gate incapable of deadlocking a cluster: the
    // refusal is bounded by audibility, so the node holding the newer policy
    // going away opens the gate rather than closing it forever.
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let mut heard = Directory::new();
    let long_ago = Instant::now();
    heard.heard("10.0.0.1:9000", running(4, 0), long_ago);
    let now = long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1);
    assert_eq!(
        heard_a_newer_policy(&declared, &heard, None, now, tessari_storage::LEASE_TTL),
        None,
        "a peer nobody has heard from in longer than a lease was still              silencing this node, so a cluster that lost the one node holding              the newer policy could never elect again"
    );
}

#[test]
fn a_policy_advertised_by_an_undeclared_address_is_not_a_member_speaking() {
    // The same rule `heard_a_leader` and `upstream` hold: a greeting from an
    // address this node's catalog does not declare is a stranger, and a
    // stranger that can silence a candidate is a denial of service with a
    // one-line implementation.
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.9:9000", running(4, 0))]);
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        None
    );
}

#[test]
fn the_newest_policy_heard_is_the_one_reported() {
    // Reported rather than merely detected, so an operator reading the log
    // line knows WHICH policy this node is behind. With several peers at
    // several stamps, the answer has to be the newest or the report names a
    // policy that is itself superseded.
    let declared = [
        named("one", "10.0.0.1:9000", [9; NODE_ID_LEN]),
        named("two", "10.0.0.2:9000", [8; NODE_ID_LEN]),
        named("three", "10.0.0.3:9000", [7; NODE_ID_LEN]),
    ];
    let heard = greeted(&[
        ("10.0.0.1:9000", running(4, 1)),
        ("10.0.0.2:9000", running(5, 0)),
        ("10.0.0.3:9000", running(4, 9)),
    ]);
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            Some(stamp(3, 0)),
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        Some(stamp(5, 0))
    );
}

#[test]
fn a_node_that_has_granted_nothing_is_answered_by_the_directory_exactly_as_before() {
    let declared = [named("leader", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.1:9000", writing(Epoch::new(7)))]);
    // `None` is *no such evidence*, never *no leader*. A follower outside
    // the deciding set grants nothing and must still be held back by a
    // greeting, or this change would make every non-voter campaign.
    assert!(
        heard_a_leader(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        "a node with no grant to read was not held back by a fresh greeting"
    );
}

#[test]
fn a_renewal_that_wins_nothing_keeps_the_lease_it_holds() {
    let held = Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from: Instant::now(),
    };
    let mut renewing = Renewing::holding(held);
    let standing = renewing.once(NODE, Instant::now(), |_, _| Stood::NotDue);
    assert_eq!(standing, held, "a round that won nothing changed the lease");
    assert_eq!(renewing.standing(), held);
}

#[test]
fn an_election_timeout_is_the_lease_plus_a_spread_that_differs_by_node() {
    // G053 SG2b, D4. Never shorter than the lease — a voter refuses all but
    // the incumbent until then — and never more than the spread past it;
    // and two nodes do not share one timeout at every epoch, which is the
    // property that keeps two followers from standing on the same tick.
    let spread = Duration::from_millis(tessari_constants::ELECTION_JITTER_MILLIS);
    let (one, other) = ([1_u8; NODE_ID_LEN], [2_u8; NODE_ID_LEN]);
    let mut differed = 0_u32;
    for epoch in 1..=64 {
        let epoch = Epoch::new(epoch);
        for node in [one, other] {
            let waited = election_timeout(node, epoch, tessari_storage::LEASE_TTL);
            assert!(waited >= tessari_storage::LEASE_TTL, "{waited:?}");
            assert!(
                waited < tessari_storage::LEASE_TTL.saturating_add(spread),
                "{waited:?}"
            );
        }
        if election_timeout(one, epoch, tessari_storage::LEASE_TTL)
            != election_timeout(other, epoch, tessari_storage::LEASE_TTL)
        {
            differed = differed.saturating_add(1);
        }
    }
    assert!(
        differed >= 60,
        "two nodes shared a timeout {} times in 64",
        64_u32.saturating_sub(differed)
    );
}

#[test]
fn a_renewal_re_asks_the_epoch_it_holds() {
    // G053 SG2b. A leader renews about every 300 ms, and an epoch per
    // renewal was a leadership record per renewal — three a second on an
    // idle cluster, eating the retained log and waking every stream. The
    // voter has always admitted the incumbent re-asking its own epoch.
    let from = Instant::now();
    let held = Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from,
    };
    let mut renewing = Renewing::holding(held);
    let stood_for = RefCell::new(Vec::new());
    let renewed = Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from: from + Duration::from_millis(300),
    };
    let standing = renewing.once(NODE, from, |_: Lease, next| {
        stood_for.borrow_mut().push(next);
        Stood::Won(renewed)
    });
    assert_eq!(*stood_for.borrow(), vec![Epoch::new(4)]);
    assert_eq!(standing, renewed, "a round that was won was not taken up");
}

#[test]
fn a_holder_told_of_a_higher_epoch_stands_above_it() {
    // The renewal keeps its epoch only while nothing says the cluster moved.
    // A refusal naming epoch 9 means a rival stood there; re-asking 4 would
    // be refused for ever, so the next stand is above what was heard.
    let from = Instant::now();
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from,
    });
    renewing.once(NODE, from, |_, _| Stood::Lost {
        granted: Epoch::new(9),
    });
    let later = from + Duration::from_secs(5);
    let stood_for = RefCell::new(Vec::new());
    renewing.once(NODE, later, |_: Lease, next| {
        stood_for.borrow_mut().push(next);
        Stood::NotDue
    });
    assert_eq!(*stood_for.borrow(), vec![Epoch::new(10)]);
}

#[test]
fn a_node_that_never_led_stands_for_a_new_epoch() {
    let from = Instant::now();
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::ZERO,
        from,
    });
    let stood_for = RefCell::new(Vec::new());
    renewing.once(NODE, from, |_: Lease, next| {
        stood_for.borrow_mut().push(next);
        Stood::NotDue
    });
    assert_eq!(*stood_for.borrow(), vec![Epoch::new(1)]);
}
