//! Expired keys are removed through the log, guided by the expiry index
//! (G035 S2.1).
//!
//! The index is asserted on its OWN keys: a read confirms every version against
//! the clock, so an entry left behind — or one never written — changes no answer
//! and is visible nowhere else.

use std::thread;

use tessari_encoding::{ExpiryKey, KeyKind, StoreKey};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{RecordId, Value};

use super::{Backend, PAST_SHORT, SHORT, keys, on_each_backend, opened, run, value};

/// The record identities the expiry index names, in instant order.
fn indexed(backend: &Backend) -> Vec<RecordId> {
    backend
        .raw
        .scan(&ScanRequest {
            keyspace: KeyKind::ExpiryIndex.keyspace(),
            range: KeyRange::prefix(&[KeyKind::ExpiryIndex.tag()]),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap()
        .into_iter()
        .map(|(key, _)| ExpiryKey::decode(key.as_slice()).unwrap().id)
        .collect()
}

#[test]
fn the_expiry_index_names_exactly_the_keys_that_expire() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let named = |ids: &[&str]| ids.iter().map(|id| RecordId::from(*id)).collect::<Vec<_>>();
        run(
            &mut session,
            "SET cache:'a' = 1 EXPIRE 1h; SET cache:'plain' = 1;",
        );
        assert_eq!(indexed(backend), named(&["a"]), "{}", backend.name);
        run(
            &mut session,
            "EXPIRE cache:'a' 2h; EXPIRE cache:'plain' 30m;",
        );
        assert_eq!(
            indexed(backend),
            named(&["plain", "a"]),
            "{}: moved, not doubled",
            backend.name
        );
        run(&mut session, "PERSIST cache:'plain';");
        assert_eq!(
            indexed(backend),
            named(&["a"]),
            "{}: persisted",
            backend.name
        );
        run(&mut session, "SET cache:'a' = 2;");
        assert_eq!(
            indexed(backend),
            named(&[]),
            "{}: a plain SET clears it",
            backend.name
        );
        run(&mut session, "SET cache:'d' = 1 EXPIRE 1h; DEL cache:'d';");
        assert_eq!(indexed(backend), named(&[]), "{}: deleted", backend.name);
    });
}

#[test]
fn the_pass_removes_what_has_expired_and_nothing_else() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            &format!(
                "SET cache:'x' = 1 EXPIRE {SHORT}; SET cache:'y' = 1 EXPIRE {SHORT}; \
                 SET cache:'z' = 1 EXPIRE {SHORT}; SET cache:'later' = 1 EXPIRE 1h; \
                 SET cache:'never' = 1;"
            ),
        );
        let nothing_yet = backend.store.remove_expired().unwrap();
        assert_eq!(
            nothing_yet.records, 0,
            "{}: nothing has passed yet",
            backend.name
        );
        thread::sleep(PAST_SHORT);
        let lapsed = backend.store.remove_expired().unwrap();
        assert_eq!(
            (lapsed.records, lapsed.stale),
            (3, 0),
            "{}: three removed, no stale entry",
            backend.name
        );
        assert_eq!(
            indexed(backend),
            vec![RecordId::from("later")],
            "{}",
            backend.name
        );
        assert_eq!(
            keys(&mut session, "KEYS FROM cache;"),
            2,
            "{}",
            backend.name
        );
        assert_eq!(value(&mut session, "GET cache:'never';"), Value::from(1));
        let again = backend.store.remove_expired().unwrap();
        assert_eq!(again.records, 0, "{}: the pass is idempotent", backend.name);
    });
}
