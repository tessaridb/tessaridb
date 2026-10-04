use super::*;

#[test]
fn a_node_that_may_write_collects_from_nobody() {
    // It is the origin of what it holds, which is exactly what
    // `current_as_of` says when it answers zero. Collecting into it would
    // apply a peer's records beside its own — the divergence the epoch chain
    // refuses at apply time, prevented here at the timer instead.
    let declared = [peer(Roles::WRITABLE, Some(NODE))];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(upstream(Roles::ALONE, &declared, &heard), None);
    assert_eq!(upstream(Roles::WRITABLE, &declared, &heard), None);
    // And the same node with the role taken away follows the same peer, so
    // the rule is the role and not something about the peer.
    assert_eq!(
        upstream(Roles::SERVING, &declared, &heard),
        Some((NODE, "10.0.0.2:9000".to_owned()))
    );
}

#[test]
fn a_joining_node_collects_from_the_seed_that_says_it_may_write_now() {
    // The one round that exists to break a circle: the membership lives in
    // the catalog, the catalog arrives by collecting from a member, and a
    // node that has just been told to join holds neither.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(
        bootstrap_from(Roles::SERVING, &seeds, &heard),
        Some((NODE, "10.0.0.2:9000".to_owned()))
    );
}

#[test]
fn a_node_that_may_write_does_not_collect_from_a_seed_either() {
    // The same rule `upstream` applies, and stated separately because the
    // two functions are siblings rather than one with a flag: a rule that
    // held in one of them and not the other would be a node that ignores
    // its peers and follows a command-line address.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(bootstrap_from(Roles::ALONE, &seeds, &heard), None);
    assert_eq!(bootstrap_from(Roles::WRITABLE, &seeds, &heard), None);
}

#[test]
fn a_seed_that_has_not_been_greeted_is_not_collected_from() {
    // Absence of a greeting is not evidence that a seed may write, and the
    // consequence here is sharper than it is for a declared peer: this node
    // holds nothing at all, so the first thing it collects is the whole of
    // what it will believe.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    assert_eq!(
        bootstrap_from(Roles::SERVING, &seeds, &Directory::new()),
        None
    );
}

#[test]
fn a_seed_that_is_a_follower_is_not_collected_from() {
    // A seed is an address an operator wrote down, and which node happens to
    // be leading is not a fact an operator can write down — so a seed
    // pointing at a node that may not write is the ordinary case, not a
    // misconfiguration. The joiner waits rather than pulling from a copy.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    let heard = greeted(&[("10.0.0.2:9000", following())]);
    assert_eq!(bootstrap_from(Roles::SERVING, &seeds, &heard), None);
}

#[test]
fn among_seeds_the_newer_leadership_is_collected_from() {
    // The same tie `upstream` breaks and for the same reason: a leader
    // demoted a moment ago and its successor can both be in this directory,
    // because a greeting is as fresh as the last awareness round.
    let seeds = [seed(NODE, "10.0.0.2:9000"), seed(ANOTHER, "10.0.0.3:9000")];
    let heard = greeted(&[
        ("10.0.0.2:9000", writing(Epoch::new(7))),
        ("10.0.0.3:9000", writing(Epoch::new(8))),
    ]);
    assert_eq!(
        bootstrap_from(Roles::SERVING, &seeds, &heard),
        Some((ANOTHER, "10.0.0.3:9000".to_owned()))
    );
}

#[test]
fn a_follower_with_no_writable_peer_collects_from_nobody() {
    assert_eq!(upstream(Roles::SERVING, &[], &Directory::new()), None);
}

#[test]
fn a_writable_peer_nobody_has_identified_cannot_be_collected_from() {
    // A peer connection demands a certificate valid for a name derived from
    // the peer's id, so an endpoint whose id nobody knows cannot be dialled
    // at all — the same wall the seed address runs into. `None` rather than
    // a half-formed attempt.
    let declared = [peer(Roles::WRITABLE, None)];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(upstream(Roles::SERVING, &declared, &heard), None);
}

#[test]
fn a_follower_collects_from_the_peer_that_says_it_may_write_now() {
    // ADR-0065, and the configuration that forced it: ADR-0063 and ADR-0064
    // together make *every coordinating node also declared writable* the
    // only shape in which a failover produces a writer, so two writable ROWS
    // is the normal cluster rather than a misconfiguration. The row says who
    // may be followed; the greeting says which of them is the origin now.
    let declared = [
        named("one", "10.0.0.2:9000", [1; NODE_ID_LEN]),
        named("two", "10.0.0.3:9000", [2; NODE_ID_LEN]),
    ];
    let heard = greeted(&[
        ("10.0.0.2:9000", following()),
        ("10.0.0.3:9000", writing(Epoch::new(4))),
    ]);
    assert_eq!(
        upstream(Roles::SERVING, &declared, &heard),
        Some(([2; NODE_ID_LEN], "10.0.0.3:9000".to_owned())),
        "both rows are declared writable; only one of them said it may write"
    );
}

#[test]
fn the_newer_leadership_wins_a_directory_holding_both() {
    // Not hypothetical. A greeting is as fresh as the last awareness round
    // and no fresher, so a leader demoted a moment ago and its successor are
    // both in this directory saying they may write. ADR-0059's ordering
    // settles it, which is the same rule a voter applies to a ballot.
    let declared = [
        named("old", "10.0.0.2:9000", [1; NODE_ID_LEN]),
        named("new", "10.0.0.3:9000", [2; NODE_ID_LEN]),
    ];
    let heard = greeted(&[
        ("10.0.0.2:9000", writing(Epoch::new(4))),
        ("10.0.0.3:9000", writing(Epoch::new(5))),
    ]);
    assert_eq!(
        upstream(Roles::SERVING, &declared, &heard),
        Some(([2; NODE_ID_LEN], "10.0.0.3:9000".to_owned()))
    );
}

#[test]
fn a_peer_this_node_has_never_greeted_is_not_followed() {
    // Absence of a greeting is not evidence that a peer may write. A cold
    // node collects from nobody until its first awareness round lands —
    // one interval of not collecting, against pulling records from whichever
    // address happened to be declared first.
    let declared = [named("one", "10.0.0.2:9000", [1; NODE_ID_LEN])];
    assert_eq!(upstream(Roles::SERVING, &declared, &Directory::new()), None);
}

#[test]
fn a_failed_collection_retries_from_the_same_position() {
    let mut collecting = Collecting::new();
    let seed = Sequence::new(5);
    let refused = collecting.once(STORE, seed, |_| Err::<Sequence, ()>(()));
    assert_eq!(
        refused,
        Err(()),
        "a failed pass answered as though it landed"
    );
    assert_eq!(collecting.reached(STORE), Some(Sequence::new(5)));

    let asked = RefCell::new(Vec::new());
    // A seed the retry must NOT take: the cursor exists now, so a pass that
    // read the seed again would be a pass that forgot where it failed.
    let reached = collecting.once(STORE, Sequence::new(99), |at| {
        asked.borrow_mut().push(at);
        Ok::<Sequence, ()>(Sequence::new(at.get() + 10))
    });
    assert_eq!(
        *asked.borrow(),
        vec![Sequence::new(5)],
        "the retry asked from somewhere other than where it failed"
    );
    assert_eq!(reached, Ok(Sequence::new(15)));
}

#[test]
fn a_collection_that_lands_advances_the_cursor() {
    let mut collecting = Collecting::new();
    assert_eq!(
        collecting.once(STORE, Sequence::new(1), |_| Ok::<Sequence, ()>(
            Sequence::new(9)
        )),
        Ok(Sequence::new(9))
    );
    assert_eq!(collecting.reached(STORE), Some(Sequence::new(9)));
}

/// The whole reason the cursor is a map: two logs, two counters.
#[test]
fn a_position_reached_in_one_log_is_not_a_position_in_another() {
    let prod = Reach::Namespace(NamespaceId::new(1));
    let mut collecting = Collecting::new();
    assert_eq!(collecting.reached(prod), None, "nothing collected anywhere");

    assert_eq!(
        collecting.once(STORE, Sequence::new(1), |_| Ok::<Sequence, ()>(
            Sequence::new(40)
        )),
        Ok(Sequence::new(40))
    );
    // The seed is what a namespace log with no cursor starts from, and the
    // store's log reaching 40 must not spend it: a single cursor would have
    // asked this log for position 40 and been answered a gap or a re-send
    // depending only on which log ran ahead.
    let asked = RefCell::new(Vec::new());
    assert_eq!(
        collecting.once(prod, Sequence::new(1), |at| {
            asked.borrow_mut().push(at);
            Ok::<Sequence, ()>(Sequence::new(3))
        }),
        Ok(Sequence::new(3))
    );
    assert_eq!(*asked.borrow(), vec![Sequence::new(1)]);
    assert_eq!(collecting.reached(STORE), Some(Sequence::new(40)));
    assert_eq!(collecting.reached(prod), Some(Sequence::new(3)));
}
