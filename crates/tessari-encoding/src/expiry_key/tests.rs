#![allow(clippy::unwrap_used)]

use super::*;

fn key(at: u64, table: u32, id: &str) -> ExpiryKey {
    ExpiryKey {
        at,
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(table),
        id: RecordId::from(id),
    }
}

#[test]
fn an_entry_round_trips() {
    let entry = key(1_700_000_000_000, 9, "abc");
    assert_eq!(ExpiryKey::decode(entry.encode().as_slice()).unwrap(), entry);
}

#[test]
fn entries_sort_by_instant_before_anything_else() {
    let early = key(5, 900, "z").encode();
    let late = key(6, 1, "a").encode();
    assert!(early.as_slice() < late.as_slice());
}

/// Whether a key falls inside a range, read off its two bounds.
fn inside(range: &KeyRange, key: &Key) -> bool {
    use std::ops::Bound;
    let above = match range.start() {
        Bound::Included(start) => key >= start,
        Bound::Excluded(start) => key > start,
        Bound::Unbounded => true,
    };
    let below = match range.end() {
        Bound::Included(end) => key <= end,
        Bound::Excluded(end) => key < end,
        Bound::Unbounded => true,
    };
    above && below
}

#[test]
fn passed_by_takes_the_instant_itself_and_nothing_later() {
    let range = ExpiryKey::passed_by(100);
    assert!(inside(&range, &key(100, 1, "a").encode()));
    assert!(inside(&range, &key(0, 1, "a").encode()));
    assert!(!inside(&range, &key(101, 1, "a").encode()));
}

#[test]
fn the_mark_is_only_a_header() {
    assert_eq!(ExpiryMark.encode().as_slice().len(), 2);
    assert_eq!(
        ExpiryMark::decode(ExpiryMark.encode().as_slice()).unwrap(),
        ExpiryMark
    );
}
