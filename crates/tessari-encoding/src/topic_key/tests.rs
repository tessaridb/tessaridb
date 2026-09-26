#![allow(clippy::unwrap_used)]

use super::*;

fn at(table: u32, offset: u64, id: &str) -> TopicOffsetKey {
    TopicOffsetKey {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(table),
        offset,
        id: RecordId::from(id),
    }
}

fn entry(id: &str, offset: u64) -> TopicEntryKey {
    TopicEntryKey {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(7),
        id: RecordId::from(id),
        offset,
    }
}

#[test]
fn both_keys_round_trip() {
    let position = at(7, 42, "m");
    assert_eq!(
        TopicOffsetKey::decode(position.encode().as_slice()).unwrap(),
        position
    );
    let filed = entry("m", 42);
    assert_eq!(
        TopicEntryKey::decode(filed.encode().as_slice()).unwrap(),
        filed
    );
}

#[test]
fn positions_sort_numerically_whatever_the_message() {
    assert!(at(7, 9, "zzz").encode().as_slice() < at(7, 10, "aaa").encode().as_slice());
    assert!(at(7, 255, "a").encode().as_slice() < at(7, 256, "a").encode().as_slice());
}

#[test]
fn a_read_from_a_position_starts_at_that_position() {
    let from =
        TopicOffsetKey::from_offset(NamespaceId::new(1), DatabaseId::new(2), TableId::new(7), 10);
    assert!(at(7, 9, "zzz").encode().as_slice() < from.as_slice());
    assert!(from.as_slice() <= at(7, 10, "").encode().as_slice());
}

#[test]
fn one_message_prefix_does_not_cover_a_longer_identity() {
    let prefix = TopicEntryKey::message_prefix(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(7),
        &RecordId::from("a"),
    );
    assert!(entry("a", 3).encode().as_slice().starts_with(&prefix));
    assert!(!entry("ab", 3).encode().as_slice().starts_with(&prefix));
}

#[test]
fn a_head_round_trips() {
    let head = TopicHeadKey {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(7),
    };
    assert_eq!(
        TopicHeadKey::decode(head.encode().as_slice()).unwrap(),
        head
    );
}
