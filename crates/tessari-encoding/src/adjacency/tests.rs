#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;

fn key(node: i64, kind: u32, direction: Direction, neighbour: i64) -> AdjacencyKey {
    AdjacencyKey::new(
        NamespaceId::new(1),
        DatabaseId::new(2),
        GraphId::new(3),
        TableId::new(4),
        RecordId::Int(node),
        EdgeKindId::new(kind),
        direction,
        TableId::new(5),
        RecordId::Int(neighbour),
    )
}

#[test]
fn an_entry_survives_the_round_trip_it_is_stored_through() {
    let original = key(1, 7, Direction::Out, 2);
    let read_back = AdjacencyKey::decode(original.encode().as_slice()).unwrap();
    assert_eq!(read_back, original);
}

#[test]
fn a_text_neighbour_round_trips_beside_an_integer_one() {
    // The four record-id variants carry a leading discriminator, so a node's
    // neighbours group by id kind before sorting within a kind. Asserted
    // rather than assumed, because a decoder that guessed the variant would
    // read a plausible wrong neighbour instead of failing.
    let mut original = key(1, 7, Direction::Out, 2);
    original.neighbour = RecordId::from("alice");
    let read_back = AdjacencyKey::decode(original.encode().as_slice()).unwrap();
    assert_eq!(read_back, original);
}

#[test]
fn entries_sort_by_node_then_kind_then_direction_then_neighbour() {
    // The ordering IS the engine: a hop is a range read only because the
    // neighbours of one node under one kind in one direction are contiguous.
    // Encoded bytes are compared rather than the struct, because it is the
    // bytes the store sorts.
    let ordered = [
        key(1, 7, Direction::Out, 1),
        key(1, 7, Direction::Out, 2),
        key(1, 7, Direction::In, 1),
        key(1, 8, Direction::Out, 1),
        key(2, 7, Direction::Out, 1),
    ];
    for pair in ordered.windows(2) {
        let earlier = pair[0].encode();
        let later = pair[1].encode();
        assert!(
            earlier.as_slice() < later.as_slice(),
            "{:?} should sort before {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn a_hop_prefix_bounds_exactly_the_entries_it_names() {
    // A prefix that also matched a neighbouring edge kind or the opposite
    // direction would make a hop return edges the caller did not ask for,
    // and nothing would report it as wrong.
    let prefix = AdjacencyKey::hop_prefix(
        NamespaceId::new(1),
        DatabaseId::new(2),
        GraphId::new(3),
        TableId::new(4),
        &RecordId::Int(1),
        EdgeKindId::new(7),
        Direction::Out,
    );
    assert!(
        key(1, 7, Direction::Out, 9)
            .encode()
            .as_slice()
            .starts_with(&prefix)
    );
    assert!(
        !key(1, 7, Direction::In, 9)
            .encode()
            .as_slice()
            .starts_with(&prefix)
    );
    assert!(
        !key(1, 8, Direction::Out, 9)
            .encode()
            .as_slice()
            .starts_with(&prefix)
    );
    assert!(
        !key(2, 7, Direction::Out, 9)
            .encode()
            .as_slice()
            .starts_with(&prefix)
    );
}

#[test]
fn a_node_prefix_covers_both_directions_and_every_kind() {
    let prefix = AdjacencyKey::node_prefix(
        NamespaceId::new(1),
        DatabaseId::new(2),
        GraphId::new(3),
        TableId::new(4),
        &RecordId::Int(1),
    );
    for entry in [
        key(1, 7, Direction::Out, 9),
        key(1, 7, Direction::In, 9),
        key(1, 8, Direction::Out, 9),
    ] {
        assert!(entry.encode().as_slice().starts_with(&prefix), "{entry:?}");
    }
    assert!(
        !key(2, 7, Direction::Out, 9)
            .encode()
            .as_slice()
            .starts_with(&prefix)
    );
}

#[test]
fn the_mirror_of_a_mirror_is_the_entry_itself() {
    // Both entries are written together, so the mirror is a function rather
    // than something each caller re-derives — and an involution is the
    // cheapest statement that it swaps every component it should.
    let original = key(1, 7, Direction::Out, 2);
    assert_eq!(original.mirror().mirror(), original);

    let mirror = original.mirror();
    assert_eq!(mirror.node, original.neighbour);
    assert_eq!(mirror.node_table, original.neighbour_table);
    assert_eq!(mirror.direction, Direction::In);
}

#[test]
fn an_unknown_direction_byte_is_refused_rather_than_defaulted() {
    // A direction that decoded to a default would turn a follower into a
    // followee with nothing in an error state.
    let error = Direction::from_tag(0x02).unwrap_err();
    assert!(
        matches!(error, Error::UnknownDirection { found: 0x02 }),
        "{error}"
    );
}

#[test]
fn properties_round_trip_and_an_edge_without_any_stays_empty() {
    let carried = EdgeProperties::new(vec![1, 2, 3]);
    assert_eq!(
        EdgeProperties::decode(carried.encode().as_slice()).unwrap(),
        carried
    );

    let bare = EdgeProperties::default();
    assert!(bare.is_empty());
    assert!(
        EdgeProperties::decode(bare.encode().as_slice())
            .unwrap()
            .is_empty()
    );
}
