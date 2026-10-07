//! A unique value held by an expired record is released to the next writer
//! (ADR-0122 A7, Q-949) — asserted on the index's own keys, because a read
//! confirms each candidate against its record and cannot see an orphan.

use std::sync::Arc;
use std::thread;
use std::time::Duration as Wait;

use tessari_encoding::{IndexTarget, KeyKind, StoreKey, StoreValue, UniqueIndexKey};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_storage::Store;
use tessari_types::RecordId;

use super::{opened, run};

const PAST_SHORT: Wait = Wait::from_millis(450);

/// Every unique entry in the store, as the identity each one names.
fn unique_holders(backend: &Arc<dyn KvBackend>) -> Vec<RecordId> {
    backend
        .scan(&ScanRequest {
            keyspace: UniqueIndexKey::keyspace(),
            range: KeyRange::prefix(&[KeyKind::UniqueIndex.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap()
        .iter()
        .map(|(_, value)| IndexTarget::decode(value.as_slice()).unwrap().id)
        .collect()
}

fn claimed_after_expiry(declaration: &str, first: &str) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = opened(&store);
    run(&mut session, declaration);
    run(
        &mut session,
        "DEFINE INDEX by_email ON users FIELDS email UNIQUE;",
    );
    run(&mut session, first);
    assert_eq!(unique_holders(&backend), vec![RecordId::Int(1)]);
    thread::sleep(PAST_SHORT);
    run(&mut session, "CREATE users:2 = { email: 'a@x' };");
    assert_eq!(
        unique_holders(&backend),
        vec![RecordId::Int(2)],
        "{declaration}: the value moved to the new record and nothing else holds it"
    );
    // The pass finds nothing left to remove and must not take the new entry.
    let lapsed = store.remove_expired().unwrap();
    assert_eq!(lapsed.stale, 0);
    assert_eq!(unique_holders(&backend), vec![RecordId::Int(2)]);
}

#[test]
fn a_table_that_declares_expiry_releases_a_unique_value_its_expired_record_held() {
    claimed_after_expiry(
        "DEFINE TABLE users (email string) EXPIRE;",
        "CREATE users:1 = { email: 'a@x' } EXPIRE 300ms;",
    );
}

#[test]
fn a_key_that_expired_releases_its_unique_value_too() {
    claimed_after_expiry(
        "DEFINE TABLE users SCHEMALESS;",
        "SET users:1 = { email: 'a@x' } EXPIRE 300ms;",
    );
}

#[test]
fn a_unique_value_a_live_record_holds_is_still_refused() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = opened(&store);
    run(
        &mut session,
        "DEFINE TABLE users (email string) EXPIRE AFTER 1h; \
         DEFINE INDEX by_email ON users FIELDS email UNIQUE; \
         CREATE users:1 = { email: 'a@x' };",
    );
    assert!(
        session
            .run("CREATE users:2 = { email: 'a@x' };")
            .unwrap_err()
            .to_string()
            .contains("by_email"),
        "the refusal names the index"
    );
}
