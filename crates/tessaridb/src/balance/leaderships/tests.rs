#![allow(clippy::unwrap_used)]

use tessari_storage::{NODE_ID_LEN, ReplicaDefinition, Roles};
use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};

use super::{LeadershipMoves, Line, Moved, plan};

const SHARD: Reach = Reach::Shard(
    NamespaceId::new(1),
    DatabaseId::new(1),
    TableId::new(9),
    ShardId::new(2),
);

fn id(n: u8) -> [u8; NODE_ID_LEN] {
    [n; NODE_ID_LEN]
}

/// A voter holding the whole store, placed on `leads`.
fn row(n: u8, leads: Option<Reach>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: format!("n{n}"),
        endpoint: format!("127.0.0.1:{n}"),
        roles: Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
        node: Some(id(n)),
        replicates: Some(Reach::Store),
        leads,
        clients: None,
        http: None,
        fingerprint: None,
        join: None,
        releasing: false,
    }
}

/// Node 0 leads the store line and the shard its row places; 1 and 2 lead
/// nothing.
fn crowded() -> (Vec<ReplicaDefinition>, Vec<Line>) {
    (
        vec![row(0, Some(SHARD)), row(1, None), row(2, None)],
        vec![(Reach::Store, Some(id(0))), (SHARD, Some(id(0)))],
    )
}

#[test]
fn the_store_leader_leading_its_placed_range_too_hands_it_to_an_idle_voter() {
    let (rows, lines) = crowded();
    assert_eq!(
        plan(&rows, &lines),
        Some(Moved {
            range: SHARD,
            from: "n0".to_owned(),
            to: "n1".to_owned(),
        })
    );
}

#[test]
fn a_difference_of_one_line_is_left_alone() {
    // Node 0 leads its placed shard, node 1 the store line, node 2 nothing:
    // moving the shard to node 2 would only swap who leads nothing.
    let rows = vec![row(0, Some(SHARD)), row(1, None), row(2, None)];
    let lines = vec![(Reach::Store, Some(id(1))), (SHARD, Some(id(0)))];
    assert_eq!(plan(&rows, &lines), None);
}

#[test]
fn a_node_still_leading_a_range_it_was_moved_off_is_left_to_finish() {
    // Node 0 was moved from the shard to another range nobody leads yet; it
    // still leads the shard and the store line. Moving the placement it holds
    // now would not lower what it leads.
    let other = Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(9),
        ShardId::new(3),
    );
    let rows = vec![row(0, Some(other)), row(1, Some(SHARD)), row(2, None)];
    let lines = vec![
        (Reach::Store, Some(id(0))),
        (SHARD, Some(id(0))),
        (other, None),
    ];
    assert_eq!(plan(&rows, &lines), None);
}

#[test]
fn a_range_is_moved_only_onto_a_voter_that_holds_it() {
    let (mut rows, lines) = crowded();
    // Node 1 holds another database only; node 2 does not vote.
    rows[1].replicates = Some(Reach::Database(NamespaceId::new(5), DatabaseId::new(5)));
    rows[2].roles = Roles::SERVING.and(Roles::WRITABLE);
    assert_eq!(plan(&rows, &lines), None);
    rows[2].roles = Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING);
    assert_eq!(
        plan(&rows, &lines).map(|moved| moved.to),
        Some("n2".to_owned())
    );
}

#[test]
fn a_voter_already_placed_elsewhere_is_not_a_target() {
    let other = Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(9),
        ShardId::new(3),
    );
    let rows = vec![
        row(0, Some(SHARD)),
        row(1, Some(other)),
        row(2, Some(other)),
    ];
    let lines = vec![
        (Reach::Store, Some(id(0))),
        (SHARD, Some(id(0))),
        (other, Some(id(1))),
    ];
    assert_eq!(plan(&rows, &lines), None);
}

#[test]
fn a_second_move_waits_out_the_spacing_after_the_first() {
    let never = LeadershipMoves::default();
    assert!(!never.waits(std::time::Duration::from_secs(3600)));
    let just = LeadershipMoves {
        last: Some(std::time::Instant::now()),
    };
    assert!(just.waits(std::time::Duration::from_secs(3600)));
    assert!(!just.waits(std::time::Duration::ZERO));
}

#[test]
fn a_placement_being_given_back_is_not_moved_to_a_voter() {
    let (mut rows, lines) = crowded();
    rows[0].releasing = true;
    assert_eq!(plan(&rows, &lines), None);
}
