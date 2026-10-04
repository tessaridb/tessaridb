use super::*;

#[test]
fn a_placed_ranges_leader_is_the_peer_whose_greeting_holds_its_line_live() {
    let declared = [
        named("a", "10.0.0.1:9000", NODE),
        named("b", "10.0.0.2:9000", ANOTHER),
    ];
    let heard = greeted(&[
        ("10.0.0.1:9000", on_a_line(NODE, shard(2), 3)),
        ("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0)),
    ]);
    assert_eq!(
        leader_of_range(shard(2), &declared, &heard),
        Some((NODE, "10.0.0.1:9000".to_owned()))
    );
    assert_eq!(leader_of_range(shard(3), &declared, &heard), None);
    // A lapsed line alone leads nothing — asked without a live greeting
    // beside it, which the choice of the highest epoch would absorb.
    let lapsed = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0))]);
    assert_eq!(leader_of_range(shard(2), &declared, &lapsed), None);
    // A greeting under somebody else's row names nobody.
    let crossed = greeted(&[("10.0.0.2:9000", on_a_line(NODE, shard(2), 3))]);
    assert_eq!(leader_of_range(shard(2), &declared, &crossed), None);
}

#[test]
fn a_node_hears_a_ranges_leader_only_on_that_ranges_line() {
    let declared = [
        named("a", "10.0.0.1:9000", NODE),
        named("me", "10.0.0.2:9000", ANOTHER),
    ];
    let now = Instant::now();
    let within = tessari_storage::LEASE_TTL;
    let heard = greeted(&[("10.0.0.1:9000", on_a_line(NODE, shard(2), 3))]);
    assert!(heard_a_leader_on(
        shard(2),
        ANOTHER,
        &declared,
        &heard,
        None,
        now,
        within
    ));
    assert!(!heard_a_leader_on(
        shard(3),
        ANOTHER,
        &declared,
        &heard,
        None,
        now,
        within
    ));
    // Its own greeting is not a leader it can hear.
    let mine = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 3))]);
    assert!(!heard_a_leader_on(
        shard(2),
        ANOTHER,
        &declared,
        &mine,
        None,
        now,
        within
    ));
    // And a grant to somebody else on the line is heard without a greeting.
    assert!(heard_a_leader_on(
        shard(3),
        ANOTHER,
        &declared,
        &Directory::new(),
        Some(now),
        now,
        within
    ));
}

#[test]
fn a_node_stands_for_the_range_its_own_row_places() {
    let mut placed = named("a", "10.0.0.1:9000", NODE);
    placed.leads = Some(shard(2));
    let declared = [placed, named("b", "10.0.0.2:9000", ANOTHER)];
    assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
    assert_eq!(stands_for(&declared, &ANOTHER), None);
}

/// Q-857. A node subscribed to one shard won the store line, answered a
/// read of the split table from its one shard, and said nothing: a leader
/// never collects, so nothing on it says it holds less than everything.
#[test]
fn a_node_subscribed_to_less_than_the_store_does_not_stand_for_the_store() {
    let mut narrow = named("a", "10.0.0.1:9000", NODE);
    narrow.replicates = Some(shard(3));
    narrow.leads = Some(shard(3));
    let mut whole = named("b", "10.0.0.2:9000", ANOTHER);
    whole.replicates = Some(Reach::Store);
    let declared = [narrow, whole];
    assert!(!stands_for_the_store(&declared, &NODE));
    assert_eq!(stands_for(&declared, &NODE), Some(shard(3)));
    assert!(stands_for_the_store(&declared, &ANOTHER));
    // A namespace is narrower than the store too.
    let mut namespace = named("a", "10.0.0.1:9000", NODE);
    namespace.replicates = Some(Reach::Namespace(NamespaceId::new(1)));
    assert!(!stands_for_the_store(&[namespace], &NODE));
    // A row with no subscription stands as it always has.
    assert!(stands_for_the_store(
        &[named("a", "10.0.0.1:9000", NODE)],
        &NODE
    ));
}

/// G034 S3.2 (Q-797). Two rows bound to one node, each placing a range:
/// the node stands for the first row's range only, so it never leads two
/// placed lines. A commit spanning two placed lines therefore cannot be
/// taken by one node, and a node's own database or namespace log is written
/// only under the store line or a coarser placement it leads — both of
/// which the collectors already ask it for.
#[test]
fn a_node_bound_to_two_placing_rows_stands_for_one_range() {
    let mut first = named("a", "10.0.0.1:9000", NODE);
    first.leads = Some(shard(2));
    let mut second = named("c", "10.0.0.1:9000", NODE);
    second.leads = Some(shard(3));
    let declared = [first, named("b", "10.0.0.2:9000", ANOTHER), second];
    assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
}

/// ADR-0098 D3. A row giving its range back still holds the line and says
/// so, but no longer campaigns; the store line's leader, placed nowhere,
/// campaigns for it instead — and a store leader with a placement of its
/// own does not, so no node ever leads two placed lines.
#[test]
fn a_released_range_is_campaigned_for_by_the_store_leader_alone() {
    let mut giving = named("a", "10.0.0.1:9000", NODE);
    giving.leads = Some(shard(2));
    giving.releasing = true;
    let declared = [giving.clone(), named("b", "10.0.0.2:9000", ANOTHER)];
    assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
    assert_eq!(campaigns_for(&declared, &NODE), None);
    assert_eq!(released(&declared), vec![shard(2)]);
    assert_eq!(campaign_line(&declared, &NODE, false), None);
    assert_eq!(campaign_line(&declared, &ANOTHER, true), Some(shard(2)));
    assert_eq!(campaign_line(&declared, &ANOTHER, false), None);
    // Placed elsewhere itself, the store leader takes nothing back.
    let mut placed = named("b", "10.0.0.2:9000", ANOTHER);
    placed.leads = Some(shard(3));
    let declared = [giving.clone(), placed];
    assert_eq!(campaign_line(&declared, &ANOTHER, true), Some(shard(3)));
    // Another row still placing the range: it is not released at all.
    let mut staying = named("b", "10.0.0.2:9000", ANOTHER);
    staying.leads = Some(shard(2));
    assert!(released(&[giving, staying]).is_empty());
}

/// G053 SG5b. A non-preferred leader hands the range to a preferred
/// candidate that is audible and caught up — and to nobody else.
#[test]
fn a_leader_yields_only_to_a_preferred_candidate_that_is_caught_up() {
    let mut leading = named("a", "10.0.0.1:9000", NODE);
    leading.leads = Some(shard(2));
    let mut preferred = named("b", "10.0.0.2:9000", ANOTHER);
    preferred.leads = Some(shard(2));
    preferred.preferred = true;
    let declared = [leading.clone(), preferred.clone()];
    let now = Instant::now();
    let within = Duration::from_secs(2);
    let mine = crate::grant::Reached {
        leadership: Epoch::new(1),
        tail: Sequence::new(3),
    };
    let level = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0))]);
    assert_eq!(
        preferred_to_yield_to(shard(2), NODE, &declared, &level, mine, (now, within)),
        Some(ANOTHER)
    );
    // Behind this node's own position: not yet.
    let ahead = crate::grant::Reached {
        leadership: Epoch::new(1),
        tail: Sequence::new(4),
    };
    assert_eq!(
        preferred_to_yield_to(shard(2), NODE, &declared, &level, ahead, (now, within)),
        None
    );
    // Not heard at all, or heard too long ago.
    assert_eq!(
        preferred_to_yield_to(
            shard(2),
            NODE,
            &declared,
            &Directory::new(),
            mine,
            (now, within)
        ),
        None
    );
    let later = now
        .checked_add(Duration::from_secs(3))
        .expect("an instant three seconds on is representable");
    assert_eq!(
        preferred_to_yield_to(shard(2), NODE, &declared, &level, mine, (later, within)),
        None
    );
    // A preferred leader yields to nobody; nor does anybody when no row
    // is preferred.
    let mut both = leading.clone();
    both.preferred = true;
    assert_eq!(
        preferred_to_yield_to(
            shard(2),
            NODE,
            &[both, preferred.clone()],
            &level,
            mine,
            (now, within)
        ),
        None
    );
    let mut plain = preferred;
    plain.preferred = false;
    assert_eq!(
        preferred_to_yield_to(
            shard(2),
            NODE,
            &[leading, plain],
            &level,
            mine,
            (now, within)
        ),
        None
    );
}
