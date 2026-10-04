use super::*;

#[test]
fn a_peer_that_did_not_answer_keeps_ageing_rather_than_vanishing() {
    // The rule the wave exists for. A peer greeted once and then silent must
    // keep the reading it already gave, so that it drifts out of tighter
    // bounds first and looser ones later. Erasing it would make its age
    // unknown, and an unknown age is outside EVERY bound — so one dropped
    // greeting would take a healthy node out of all routing at once.
    let heard_at = Instant::now();
    let mut directory = one_peer(heard_at);
    let later = heard_at
        .checked_add(Duration::from_secs(30))
        .expect("the clock moves forward");

    let dialled = RefCell::new(Vec::new());
    let reached = directory.greet_round(
        &[declared(1, "two.example:9080", Some(ANOTHER))],
        &ONE,
        later,
        greeter(&dialled, &["two.example:9080"]),
    );

    assert_eq!(reached, 0, "the peer refused, so nothing was reached");
    assert_eq!(
        directory.age_of("two.example:9080", later),
        Some(Duration::from_secs(35)),
        "the silent peer's reading should have aged, not vanished"
    );
}

#[test]
fn one_silent_peer_does_not_end_the_round() {
    // The peer after the failing one is the one most likely to still be
    // serving, so a round that returned at the first refusal would punish
    // every peer for the misfortune of being declared later.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[
            declared(1, "silent.example:9080", Some(ANOTHER)),
            declared(2, "awake.example:9080", Some(THIRD)),
        ],
        &ONE,
        now,
        greeter(&dialled, &["silent.example:9080"]),
    );

    assert_eq!(reached, 1, "one of the two answered");
    assert_eq!(
        dialled.borrow().as_slice(),
        ["silent.example:9080", "awake.example:9080"],
        "both peers should have been attempted"
    );
    assert!(
        directory.at("awake.example:9080").is_some(),
        "the peer after the failing one should have been recorded"
    );
}

#[test]
fn a_peer_whose_row_names_no_node_is_not_dialled() {
    // Opening a session derives the peer's TLS name from its generated
    // identifier, so a row that names no node cannot be reached at all. The
    // round skips it rather than refusing the pass: it cannot invent an id,
    // and one incomplete declaration must not disable routing for everyone.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[
            declared(1, "unbound.example:9080", None),
            declared(2, "bound.example:9080", Some(ANOTHER)),
        ],
        &ONE,
        now,
        greeter(&dialled, &[]),
    );

    assert_eq!(reached, 1, "only the bound row was diallable");
    assert_eq!(
        dialled.borrow().as_slice(),
        ["bound.example:9080"],
        "the unbound row should never have been dialled"
    );
    assert!(
        directory.at("unbound.example:9080").is_none(),
        "a row that was never dialled has nothing to record"
    );
}

#[test]
fn a_node_does_not_greet_itself() {
    // This node's own row sits in the same catalog as everybody else's. Its
    // currency is already known directly, so dialling itself is a round trip
    // to learn what the store answers for free — and it would put this node
    // in its own directory, where `read_within`'s *here first* rule has
    // already decided it does not belong.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[
            declared(1, "me.example:9080", Some(ONE)),
            declared(2, "other.example:9080", Some(ANOTHER)),
        ],
        &ONE,
        now,
        greeter(&dialled, &[]),
    );

    assert_eq!(reached, 1, "only the other node was greeted");
    assert_eq!(
        dialled.borrow().as_slice(),
        ["other.example:9080"],
        "this node should not have dialled itself"
    );
    assert!(
        directory.at("me.example:9080").is_none(),
        "this node must stay out of its own directory"
    );
}

#[test]
fn a_round_records_what_each_peer_said_against_the_instant_it_was_heard() {
    // The round's whole product. A greeting recorded without its instant is
    // a claim with no date, and the ageing rule has nothing to work from.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[declared(1, "two.example:9080", Some(ANOTHER))],
        &ONE,
        now,
        greeter(&dialled, &[]),
    );

    assert_eq!(reached, 1);
    let heard = directory
        .at("two.example:9080")
        .expect("the peer answered, so it was recorded");
    assert_eq!(heard.said.node, ANOTHER, "the greeting names who is there");
    assert_eq!(heard.at, now, "recorded against the instant it was heard");
    assert_eq!(
        directory.age_of("two.example:9080", now),
        Some(Duration::from_secs(1)),
        "and the reading is usable the moment it lands"
    );
}

#[test]
fn a_joining_node_greets_its_seeds_because_its_catalog_names_nobody() {
    // The bootstrap round. A node just told to join holds no replica rows,
    // so `greet_round` would dial nobody at all and this node would never
    // learn anything about anything.
    let seeds = [
        crate::joining::Seed {
            node: ANOTHER,
            endpoint: "10.0.0.2:9000".to_owned(),
        },
        crate::joining::Seed {
            node: THIRD,
            endpoint: "10.0.0.3:9000".to_owned(),
        },
    ];
    let mut directory = Directory::new();
    let now = Instant::now();
    let reached = directory.greet_seeds(&seeds, &ONE, now, |_endpoint, node| {
        Ok::<_, ()>(said(node, Some(Duration::ZERO), true))
    });
    assert_eq!(reached, 2, "both seeds answered");
    assert!(directory.at("10.0.0.2:9000").is_some());
    assert!(directory.at("10.0.0.3:9000").is_some());
}

#[test]
fn a_node_seeded_with_itself_does_not_dial_itself() {
    // An operator who seeds a node with its own address has written a loop.
    // Dialling it would put this node in its own directory, where the
    // *here first* rule has already decided it does not belong.
    let seeds = [crate::joining::Seed {
        node: ONE,
        endpoint: "10.0.0.1:9000".to_owned(),
    }];
    let mut directory = Directory::new();
    let reached = directory.greet_seeds(&seeds, &ONE, Instant::now(), |_endpoint, node| {
        Ok::<_, ()>(said(node, Some(Duration::ZERO), true))
    });
    assert_eq!(reached, 0, "the one seed was this node");
    assert!(directory.at("10.0.0.1:9000").is_none());
}

#[test]
fn a_seed_that_does_not_answer_leaves_the_directory_as_it_was() {
    // One failure does not end the round and nothing is erased — the same
    // rule the declared round follows, stated here because a joining node
    // has no earlier reading to keep and the count is all it has.
    let seeds = [crate::joining::Seed {
        node: ANOTHER,
        endpoint: "10.0.0.2:9000".to_owned(),
    }];
    let mut directory = Directory::new();
    let reached = directory.greet_seeds(&seeds, &ONE, Instant::now(), |_, _| Err::<Hello, ()>(()));
    assert_eq!(reached, 0);
    assert!(directory.at("10.0.0.2:9000").is_none());
}
