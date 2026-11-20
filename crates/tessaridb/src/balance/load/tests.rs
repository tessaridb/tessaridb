#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tessari_storage::{
    Balancer, Failover, NODE_ID_LEN, ReplicaDefinition, Roles, SampledShard, SampledTable,
};
use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};

use super::plan_load;
use crate::balance::Moved;

const SHARD: Reach = Reach::Shard(
    NamespaceId::new(1),
    DatabaseId::new(1),
    TableId::new(9),
    ShardId::new(2),
);
const OTHER: Reach = Reach::Shard(
    NamespaceId::new(1),
    DatabaseId::new(1),
    TableId::new(9),
    ShardId::new(3),
);

fn id(n: u8) -> [u8; NODE_ID_LEN] {
    [n; NODE_ID_LEN]
}

fn policy() -> Balancer {
    Balancer::stated(
        150,
        Duration::from_secs(30),
        Failover::DEFAULT.lease(),
        &Failover::DEFAULT,
    )
    .expect("a policy inside every relation")
}

/// A coordinating voter holding the whole store, placed on `leads`.
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
        kafka: None,
        fingerprint: None,
        join: None,
        releasing: false,
        preferred: false,
        region: None,
        domain: None,
    }
}

/// The same voter drained: its roles cleared, the spec's *Draining this
/// node*, still holding its data and its placement.
fn drained(n: u8, leads: Option<Reach>) -> ReplicaDefinition {
    ReplicaDefinition {
        roles: Roles::NONE,
        ..row(n, leads)
    }
}

/// The sampled rates the split pass is taken to have published: one table's
/// shards at `rates`, keyed so each line addresses its own shard.
fn sampled(rates: &[(Reach, u64)]) -> Vec<(TableId, SampledTable)> {
    vec![(
        TableId::new(9),
        SampledTable {
            name: "prod.shop.orders".to_owned(),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            shards: [SHARD, OTHER]
                .into_iter()
                .map(|line| {
                    let Reach::Shard(_, _, _, shard) = line else {
                        unreachable!("the fixture lines are shards")
                    };
                    SampledShard {
                        shard,
                        records: 10,
                        complete: true,
                        writes_per_second: rates
                            .iter()
                            .find_map(|(held, rate)| (*held == line).then_some(*rate)),
                    }
                })
                .collect(),
            last_act: None,
        },
    )]
}

#[test]
fn a_drained_voters_placement_moves_without_any_margin() {
    // The drain is its own branch: the load signal reads a draining node as
    // quietest, which is the direction it is being emptied in, so emptiness
    // moves regardless of the margin — and the spare store line the drained
    // node also leads is the failover policy's domain, not this act's.
    let rows = vec![drained(0, Some(SHARD)), row(1, None), row(2, None)];
    let lines = vec![(Reach::Store, Some(id(0))), (SHARD, Some(id(0)))];
    assert_eq!(
        plan_load(&rows, &lines, &policy(), &[]),
        Some(Moved {
            range: SHARD,
            from: "n0".to_owned(),
            to: "n1".to_owned(),
        })
    );
}

#[test]
fn a_drained_node_leading_only_the_store_line_moves_nothing() {
    // The store line is the failover policy's domain: a placement move can
    // only move what a peer row places, and this node places nothing.
    let rows = vec![drained(0, None), row(1, None), row(2, None)];
    let lines = vec![(Reach::Store, Some(id(0)))];
    assert_eq!(plan_load(&rows, &lines, &policy(), &[]), None);
}

#[test]
fn a_voter_carrying_two_hot_lines_sheds_one_to_an_idle_voter() {
    // The margin holds (600 to 0) and the no-overshoot bound holds exactly:
    // the shed range is half the gap, so it lands beside the idle voter
    // without crossing it.
    let rows = vec![row(0, Some(SHARD)), row(1, None), row(2, None)];
    let lines = vec![
        (Reach::Store, Some(id(0))),
        (SHARD, Some(id(0))),
        (OTHER, Some(id(0))),
    ];
    let sampled = sampled(&[(SHARD, 300), (OTHER, 300)]);
    assert_eq!(
        plan_load(&rows, &lines, &policy(), &sampled),
        Some(Moved {
            range: SHARD,
            from: "n0".to_owned(),
            to: "n1".to_owned(),
        })
    );
}

#[test]
fn a_single_hot_range_is_not_moved_to_an_idle_voter() {
    // The no-overshoot bound: one hot line on a voter is not shed — moving it
    // whole to an idle voter would make the idle voter the busiest, and the
    // range would move back next pass, one spacing window apart, forever.
    let rows = vec![row(0, Some(SHARD)), row(1, None), row(2, None)];
    let lines = vec![(Reach::Store, Some(id(0))), (SHARD, Some(id(0)))];
    let sampled = sampled(&[(SHARD, 300)]);
    assert_eq!(plan_load(&rows, &lines, &policy(), &sampled), None);
}

#[test]
fn a_voter_with_no_measured_load_moves_nothing() {
    // The margin gate: nothing measured anywhere means the busiest voter's
    // total is zero, and zero does not exceed any margin — the pass declines
    // rather than moving placements on count-shaped guesswork, which is the
    // count pass's job beside it.
    let rows = vec![row(0, Some(SHARD)), row(1, None), row(2, None)];
    let lines = vec![(Reach::Store, Some(id(0))), (SHARD, Some(id(0)))];
    assert_eq!(plan_load(&rows, &lines, &policy(), &[]), None);
}

#[test]
fn a_releasing_placement_is_left_to_its_hand_back() {
    // The operator's own machinery is moving this range; the load pass keeps
    // the count pass's hands-off rule.
    let mut giving = row(0, Some(SHARD));
    giving.releasing = true;
    let rows = vec![giving, row(1, None), row(2, None)];
    let lines = vec![
        (Reach::Store, Some(id(0))),
        (SHARD, Some(id(0))),
        (OTHER, Some(id(0))),
    ];
    let sampled = sampled(&[(SHARD, 300), (OTHER, 300)]);
    assert_eq!(plan_load(&rows, &lines, &policy(), &sampled), None);
}

#[test]
fn the_bound_holds_exactly_at_half_the_gap() {
    // Two lines at 240 each: the shed range is exactly half the gap, and the
    // bound is inclusive — the move lands beside the idle voter without
    // crossing it, which is the boundary the single-hot-range row above is
    // the far side of.
    let rows = vec![row(0, Some(SHARD)), row(1, None), row(2, None)];
    let lines = vec![
        (Reach::Store, Some(id(0))),
        (SHARD, Some(id(0))),
        (OTHER, Some(id(0))),
    ];
    let sampled = sampled(&[(SHARD, 240), (OTHER, 240)]);
    assert_eq!(
        plan_load(&rows, &lines, &policy(), &sampled),
        Some(Moved {
            range: SHARD,
            from: "n0".to_owned(),
            to: "n1".to_owned(),
        })
    );
}
