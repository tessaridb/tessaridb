#![allow(clippy::unwrap_used)]

use super::*;

fn key(table: u32, version: u64, id: &str) -> ModifiedKey {
    ModifiedKey {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(table),
        version: Sequence::new(version),
        id: RecordId::from(id),
    }
}

#[test]
fn an_entry_round_trips() {
    let entry = key(7, 42, "k");
    assert_eq!(
        ModifiedKey::decode(entry.encode().as_slice()).unwrap(),
        entry
    );
}

#[test]
fn within_a_space_the_oldest_version_sorts_first_whatever_the_key() {
    assert!(key(7, 5, "zzz").encode().as_slice() < key(7, 6, "aaa").encode().as_slice());
}

#[test]
fn every_entry_of_a_space_sits_under_its_prefix_and_no_other_spaces() {
    let prefix =
        ModifiedKey::table_prefix(NamespaceId::new(1), DatabaseId::new(2), TableId::new(7));
    assert!(key(7, 1, "a").encode().as_slice().starts_with(&prefix));
    assert!(!key(8, 1, "a").encode().as_slice().starts_with(&prefix));
}
