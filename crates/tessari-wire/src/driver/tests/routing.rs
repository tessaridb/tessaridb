use super::*;

#[test]
fn the_published_directory_answers_the_routing_question_a_read_asks() {
    // The join this whole module was built for. Until this wave the rounds
    // were written and read by nobody, so the test asserts the *reading*:
    // what a session gets back when it asks the published answer, not what
    // the greeting side put there.
    use tessari_session::Elsewhere as _;

    let mut directory = Directory::new();
    directory.heard("two.example:9080", said(), Instant::now());
    let published = Published::holding(directory);

    let found = published
        .within(Duration::from_secs(30))
        .expect("a peer one second behind is within thirty");
    assert_eq!(found.endpoint, "two.example:9080");
    assert_eq!(
        found.node, NODE,
        "the redirect must carry who is there, or it cannot be checked on arrival"
    );
}

#[test]
fn a_peer_beyond_the_bound_is_not_a_peer_the_routing_question_offers() {
    // §C-05's *exclude, never mark*, at the surface a read actually asks.
    // An implementation that answered with its freshest peer regardless
    // would turn the bound from a promise back into a hope, and the caller
    // has no way to tell the two apart.
    use tessari_session::Elsewhere as _;

    let mut directory = Directory::new();
    directory.heard("two.example:9080", said(), Instant::now());
    let published = Published::holding(directory);

    assert!(
        published.within(Duration::from_millis(500)).is_none(),
        "a copy a second behind was offered to a read that would take half of one"
    );
}

#[test]
fn a_node_that_has_greeted_nobody_offers_nowhere() {
    // The single-node case, which is every deployment that was never told
    // about peers. It must answer *not that I know of* rather than
    // inventing a candidate, because the caller turns that answer straight
    // into a refusal.
    use tessari_session::Elsewhere as _;

    let published = Published::holding(Directory::new());
    assert!(published.within(Duration::from_secs(86_400)).is_none());
}

#[test]
fn a_greeting_round_carries_previous_readings_forward() {
    let published = Published::holding(Directory::new());
    let first = Instant::now();
    published.round(|directory| directory.heard("one:9080", said(), first));

    published.round(|directory| directory.heard("two:9080", said(), first));

    let current = published.current();
    assert!(
        current.age_of("one:9080", first).is_some(),
        "the peer heard in the first round vanished in the second"
    );
    assert!(current.age_of("two:9080", first).is_some());
}

#[test]
fn a_greeting_round_publishes_nothing_until_it_is_done() {
    let published = Published::holding(Directory::new());
    let at = Instant::now();
    let during = RefCell::new(None);
    published.round(|directory| {
        directory.heard("one:9080", said(), at);
        *during.borrow_mut() = Some(Arc::clone(&published.current()));
    });
    let seen = during.borrow().clone().expect("the round ran");
    assert!(
        seen.age_of("one:9080", at).is_none(),
        "a reader saw a half-finished round"
    );
    assert!(published.current().age_of("one:9080", at).is_some());
}

#[test]
fn an_empty_catalog_names_no_peer() {
    assert!(!names_a_peer(&[], &NODE));
}

#[test]
fn a_catalog_naming_another_node_names_a_peer() {
    let declared = [named("leader", "10.0.0.2:9000", ANOTHER)];
    assert!(names_a_peer(&declared, &NODE));
}

/// The shape W260 found, and the reason this predicate is not `is_empty`.
///
/// A cluster admits a newcomer by writing a row that describes the
/// NEWCOMER, so the first row the newcomer ever collects is its own. It
/// names a member — itself — and answers nothing about who to follow, since
/// `upstream` and `greet_round` both skip it.
#[test]
fn a_catalog_holding_only_this_nodes_own_row_names_no_peer() {
    let declared = [named("joiner", "10.0.0.9:9000", NODE)];
    assert!(!names_a_peer(&declared, &NODE));
}

#[test]
fn a_row_naming_no_node_names_no_peer() {
    let declared = [peer(Roles::WRITABLE, None)];
    assert!(!names_a_peer(&declared, &NODE));
}

#[test]
fn one_row_for_somebody_else_is_enough_beside_this_nodes_own() {
    let declared = [
        named("joiner", "10.0.0.9:9000", NODE),
        named("leader", "10.0.0.2:9000", ANOTHER),
    ];
    assert!(names_a_peer(&declared, &NODE));
}

#[test]
fn a_routing_answer_carries_the_epoch_the_named_node_claimed() {
    // The value a redirect is DATED by, and the one thing this node could
    // not have produced from its own state: `said()` claims epoch 7 and this
    // node holds no leadership at all. An implementation that reached for
    // `Db::leading()` here would answer `None` and have to invent a
    // placeholder, which is how an undateable redirect gets shipped looking
    // exactly like a dated one.
    use tessari_session::Elsewhere as _;

    let mut directory = Directory::new();
    directory.heard("10.0.0.2:9000", said(), Instant::now());
    let published = Published::holding(directory);

    let peer = published
        .within(Duration::from_secs(30))
        .expect("a serving peer one second behind is inside a thirty-second bound");

    assert_eq!(peer.endpoint, "10.0.0.2:9000");
    assert_eq!(peer.node, NODE);
    assert_eq!(
        peer.epoch,
        Epoch::new(7),
        "the routing answer lost the leadership the named node published"
    );
}

// ---- G032 S4: a placed range's leader, heard and followed ---------------
