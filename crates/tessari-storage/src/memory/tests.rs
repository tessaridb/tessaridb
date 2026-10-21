#![allow(clippy::unwrap_used)]

use tessari_encoding::{ExpiryKey, ModifiedKey, RecordKey, StoreKey};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use super::*;

const NAMESPACE: NamespaceId = NamespaceId::new(3);
const DATABASE: DatabaseId = DatabaseId::new(4);

fn record(table: u32) -> Vec<u8> {
    RecordKey::new(
        NAMESPACE,
        DATABASE,
        TableId::new(table),
        RecordId::from("k"),
        Sequence::new(7),
    )
    .encode()
    .as_slice()
    .to_vec()
}

fn expiry(table: u32) -> Vec<u8> {
    ExpiryKey {
        at: 1_700_000_000_000,
        namespace: NAMESPACE,
        database: DATABASE,
        table: TableId::new(table),
        id: RecordId::from("k"),
    }
    .encode()
    .as_slice()
    .to_vec()
}

fn count(table: u32) -> Vec<u8> {
    RecordKey::new(
        SYSTEM_NAMESPACE,
        SYSTEM_DATABASE,
        RECORD_COUNTS,
        RecordId::Int(i64::from(table)),
        Sequence::new(7),
    )
    .encode()
    .as_slice()
    .to_vec()
}

#[test]
fn only_the_keys_of_a_held_table_are_routed_to_memory() {
    let held = HeldSpaces::default();
    assert!(held.routes(&record(40)).is_none(), "nothing is held yet");
    held.hold(NAMESPACE, DATABASE, TableId::new(40));
    assert!(held.routes(&record(40)).is_some());
    assert!(held.routes(&expiry(40)).is_some());
    assert!(held.routes(&count(40)).is_some());
    assert!(
        held.routes(&ModifiedKey::table_prefix(
            NAMESPACE,
            DATABASE,
            TableId::new(40)
        ))
        .is_some()
    );
    // The neighbours stay on disk: another table, its count, a table-less prefix.
    assert!(held.routes(&record(41)).is_none());
    assert!(held.routes(&expiry(41)).is_none());
    assert!(held.routes(&count(41)).is_none());
    assert!(held.routes(&count(400)).is_none());
    // The same table identifier in another database — a catalog table shares it.
    let elsewhere = RecordKey::new(
        SYSTEM_NAMESPACE,
        SYSTEM_DATABASE,
        TableId::new(40),
        RecordId::from("k"),
        Sequence::new(7),
    );
    assert!(held.routes(elsewhere.encode().as_slice()).is_none());
    assert!(held.routes(&[KeyKind::Record.tag()]).is_none());
    assert!(held.routes(&[]).is_none());
}
