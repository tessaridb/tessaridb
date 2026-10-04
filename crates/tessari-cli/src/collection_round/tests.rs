use tessari_storage::ReplicaDefinition;
use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};

use super::on_the_store_line;

/// A row naming `node`, placed to lead `leads`.
fn row(node: Option<[u8; 16]>, leads: Option<Reach>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: "peer".to_owned(),
        endpoint: "10.0.0.2:9000".to_owned(),
        roles: tessari_storage::Roles::WRITABLE,
        node,
        replicates: None,
        leads,
        clients: None,
        http: None,
        fingerprint: None,
        join: None,
        releasing: false,
        preferred: false,
        region: None,
    }
}

fn shard(id: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        ShardId::new(id),
    )
}

#[test]
fn a_candidate_catches_up_from_the_holders_of_its_range_and_no_one_else() {
    let (me, leader, holder, stranger) = ([1; 16], [2; 16], [3; 16], [4; 16]);
    let holding = |node, over| ReplicaDefinition {
        replicates: over,
        ..row(Some(node), None)
    };
    let declared = [
        holding(me, Some(Reach::Store)),
        holding(leader, Some(shard(2))),
        holding(holder, Some(Reach::Store)),
        holding(stranger, Some(shard(1))),
        holding([5; 16], None),
    ];
    let found: Vec<_> = super::holders_of(shard(2), &declared, (me, leader))
        .into_iter()
        .map(|(node, _)| node)
        .collect();
    assert_eq!(found, vec![holder]);
}

#[test]
fn the_store_line_does_not_carry_a_placed_range() {
    let declared = [
        row(None, Some(shard(1))),
        row(None, Some(shard(2))),
        row(None, None),
    ];
    let held = vec![
        Reach::Store,
        Reach::Namespace(NamespaceId::new(1)),
        Reach::Database(NamespaceId::new(1), DatabaseId::new(1)),
        shard(1),
        shard(2),
        shard(3),
    ];
    assert_eq!(
        on_the_store_line(held.clone(), &declared),
        vec![held[0], held[1], held[2], shard(3)],
        "a shard a placement carves out is collected from its own leader"
    );
    assert_eq!(on_the_store_line(held.clone(), &[]), held);
}
