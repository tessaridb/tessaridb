use super::*;

#[test]
fn the_leader_of_a_line_is_the_peer_heard_leading_it_at_the_newest_epoch() {
    let shard = tessari_types::Reach::Shard(
        tessari_types::NamespaceId::new(1),
        tessari_types::DatabaseId::new(1),
        tessari_types::TableId::new(1),
        tessari_types::ShardId::new(1),
    );
    let line = |leading: u64| crate::peer::Line {
        range: shard,
        leading: Epoch::new(leading),
        tail: Sequence::new(1),
        tail_leadership: Epoch::new(1),
    };
    let mut directory = Directory::new();
    let now = Instant::now();
    // An old leader still greeting at the epoch it lost, the new one, and
    // a candidate whose lease lapsed: only the newest live lease leads.
    directory.heard(
        "one:9000",
        Hello {
            line: Some(line(3)),
            ..said(ONE, None, true)
        },
        now,
    );
    directory.heard(
        "another:9000",
        Hello {
            line: Some(line(4)),
            ..said(ANOTHER, None, true)
        },
        now,
    );
    directory.heard(
        "third:9000",
        Hello {
            line: Some(line(0)),
            ..said(THIRD, None, true)
        },
        now,
    );
    assert_eq!(
        directory.leading(shard),
        Some(("another:9000".to_owned(), ANOTHER, Epoch::new(4)))
    );
    assert_eq!(directory.leading(tessari_types::Reach::Store), None);
}

#[test]
fn a_remembered_reading_ages_with_the_clock() {
    // The rule this module exists for. The peer said five seconds; thirty
    // seconds later its copy is thirty-five seconds old, not five. A router
    // that answered five would be measuring staleness with a stale
    // measurement, which is the failure the bound exists to prevent.
    let heard_at = Instant::now();
    let directory = one_peer(heard_at);

    assert_eq!(
        directory.age_of("two.example:9080", heard_at),
        Some(Duration::from_secs(5)),
        "at the instant it was heard, the age is what the peer said"
    );
    assert_eq!(
        directory.age_of(
            "two.example:9080",
            heard_at
                .checked_add(Duration::from_secs(30))
                .expect("thirty seconds after an instant this process made")
        ),
        Some(Duration::from_secs(35)),
        "the remembered reading did not age"
    );
    assert_eq!(
        directory.age_of("nobody.example:9080", heard_at),
        None,
        "an address nobody has greeted from reported an age"
    );
}

#[test]
fn a_directory_of_followers_knows_of_no_leader() {
    // The answer that matters most, because the alternative is a read that
    // asked for the leader being sent to a node that never claimed to be
    // one. `None` here becomes a refusal upstream, which is the honest end.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        said(ANOTHER, Some(Duration::ZERO), true),
        heard_at,
    );

    assert_eq!(
        directory.writable(),
        None,
        "a peer at zero lag was read as a leader; level is not authoritative"
    );
}

#[test]
fn the_peer_that_claims_the_writable_role_is_the_one_named() {
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        said(ANOTHER, Some(Duration::ZERO), true),
        heard_at,
    );
    directory.heard("three.example:9080", leads(THIRD, 9), heard_at);

    assert_eq!(
        directory.writable(),
        Some(("three.example:9080".to_owned(), THIRD)),
        "both halves travel, or the redirect cannot be checked on arrival"
    );
}

#[test]
fn two_peers_claiming_the_leadership_resolve_to_the_later_epoch() {
    // Not a malformed directory: this is what a handover looks like from
    // outside while one greeting is newer than the other. The higher epoch
    // is the later claim.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard("two.example:9080", leads(ANOTHER, 7), heard_at);
    directory.heard("three.example:9080", leads(THIRD, 9), heard_at);

    assert_eq!(
        directory.writable(),
        Some(("three.example:9080".to_owned(), THIRD)),
    );
}

#[test]
fn an_equal_epoch_resolves_the_same_way_twice() {
    // The map is ordered, so a tie has to break deterministically or the
    // same question answers differently on two runs.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard("three.example:9080", leads(THIRD, 9), heard_at);
    directory.heard("two.example:9080", leads(ANOTHER, 9), heard_at);

    assert_eq!(
        directory.writable(),
        Some(("three.example:9080".to_owned(), THIRD)),
        "a tie went to the lexicographically later endpoint"
    );
}

#[test]
fn a_peer_that_cannot_say_how_old_its_copy_is_is_outside_every_bound() {
    // `None` is a real answer, not an omission, and it gets the same
    // treatment the local `None` already gets: excluded, never marked.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard("two.example:9080", said(ANOTHER, None, true), heard_at);

    assert_eq!(directory.age_of("two.example:9080", heard_at), None);
    assert_eq!(
        directory.read_within(None, Duration::from_secs(86_400), heard_at),
        Destination::Nowhere,
        "a copy of unknown age was admitted by a bound of a whole day, which \
             would make the bound a formality rather than a guarantee"
    );
}
