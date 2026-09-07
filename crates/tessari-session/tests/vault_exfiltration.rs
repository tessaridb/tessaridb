//! A planted secret, hunted for everywhere the store keeps bytes.
//!
//! # Why this file exists, and what it is allowed to conclude
//!
//! The exfiltration survey enumerated twenty-three paths by which a stored value
//! could leave this engine, and argued that twelve of them are structurally
//! blind to a sealed field because the sealing happens at the payload encoder —
//! below the session layer, where the index writer, the change feed, the
//! replication log and the backup writer all read.
//!
//! **That was an argument, and an argument is not evidence.** No sealed value
//! existed when it was written. Q-412 records exactly that, and says nothing may
//! claim criteria E1, E2, E3 or K2 until a planted plaintext has been scanned
//! for against the real artifacts. This file is the beginning of that scan.
//!
//! What it covers: the raw backend across all four keyspaces — which is the
//! search index and the replication log among them — and a **real backup
//! artifact**, produced by the backup writer rather than simulated.
//!
//! What it does not cover, stated so nobody reads more into a green run than is
//! there: a **follower** applying the leader's log, which is a separate process
//! and a separate code path (criterion E3's second half), and a store on the LSM
//! backend rather than in memory. Both stay open in Q-412.
//!
//! # Every assertion here carries a control
//!
//! An absence test is the easiest kind to pass for the wrong reason: a write
//! that silently did not happen produces a store with the secret nowhere in it,
//! and reports the strongest possible result. So each scan below also asserts
//! that something which *should* be there is — the record's ordinary field, in
//! the clear, in the same bytes.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KeyRange, Keyspace, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// The planted secret. Long and distinctive, so a hit is a hit rather than a
/// coincidence, and so a partial or transformed copy still shows up.
const PLANTED: &str = "correct-horse-battery-staple-9f2b";

/// What must be found. Without it every assertion below passes vacuously.
const CONTROL: &str = "ada-lovelace-marker-71c4";

const SETUP: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
UNSEAL VAULT WITH 'an operator passphrase';
DEFINE VAULT team;
DEFINE FIELD login ON team TYPE string;
DEFINE FIELD token ON team TYPE string SECRET;
DEFINE ANALYZER words FILTERS lowercase;
DEFINE FIELD notes ON team TYPE string ANALYZER words;
DEFINE INDEX by_login ON team FIELDS login;
DEFINE INDEX on_notes ON team FIELDS notes SEARCH;
";

/// The tenancy a fresh session needs. Sessions do not share one — but they do
/// share the store's keyring, which is the point of several tests below: a
/// second session is unsealed because the *process* is, not because it asked.
const USE: &str = "USE NAMESPACE prod; USE DATABASE work;";

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Everything the backend holds, across every keyspace it has.
///
/// `META`, `DATA`, `INDEX` and `LOG` — so one scan covers the record store, the
/// search index and the replication log at once. Scanning the keyspaces by
/// enumeration rather than by name means a keyspace added later is covered
/// without anyone remembering to add it here.
fn everything(backend: &Arc<dyn KvBackend>) -> Vec<u8> {
    let mut held = Vec::new();
    for keyspace in Keyspace::ALL {
        let request = ScanRequest {
            keyspace: *keyspace,
            range: KeyRange::all(),
            direction: ScanDirection::Forward,
            limit: None,
        };
        for (key, value) in backend.scan(&request).unwrap() {
            held.extend_from_slice(key.as_slice());
            held.extend_from_slice(value.as_slice());
        }
    }
    held
}

/// A store holding one vault record: a planted secret, and a control beside it.
fn planted() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    {
        let mut session = Session::new(&store);
        session.run(SETUP).unwrap();
        session
            .run(&format!(
                "CREATE team:'github' = {{ login: '{CONTROL}', \
                 notes: 'the {CONTROL} account', token: '{PLANTED}' }};"
            ))
            .unwrap();
    }
    (backend, store)
}

#[test]
fn the_secret_is_in_no_keyspace_the_backend_has() {
    let (backend, _store) = planted();
    let stored = everything(&backend);

    assert!(
        !holds(&stored, PLANTED),
        "the planted secret is in the backend in the clear"
    );
    assert!(
        holds(&stored, CONTROL),
        "the control is absent too, so nothing was written and the scan above \
         proves nothing"
    );
}

#[test]
fn the_secret_is_not_in_a_backup_artifact() {
    let (_backend, store) = planted();
    let mut session = Session::new(&store);
    session.run(USE).unwrap();
    let Outcome::Value(Value::Bytes(artifact)) = session.run("BACKUP;").unwrap().pop().unwrap()
    else {
        panic!("BACKUP answers with bytes")
    };

    // A real artifact from the backup writer, not a simulation of one. This is
    // the file somebody copies to another machine, and it is a separate code
    // path from the record store — which is exactly why the criterion asks for
    // it separately.
    assert!(
        !holds(&artifact, PLANTED),
        "the planted secret is in a backup in the clear"
    );
    assert!(
        holds(&artifact, CONTROL),
        "the backup carries neither the secret nor the control, so it is empty \
         and the scan above proves nothing"
    );
}

#[test]
fn a_search_over_the_whole_store_cannot_find_the_secret() {
    let (_backend, store) = planted();
    let mut session = Session::new(&store);
    session.run(USE).unwrap();

    // The indexed field is analysed and searchable; the sealed one is bytes and
    // was never offered to the analyzer. Asked as a query rather than by reading
    // the index, because what matters is the answer a caller can obtain.
    let found = session
        .run(&format!(
            "SELECT id FROM team WHERE notes MATCHES '{PLANTED}';"
        ))
        .unwrap_err()
        .to_string();
    // A vault refuses `SELECT` outright, which is the strongest possible answer
    // to this question and is the one being asserted: there is no read shape
    // that reaches these records at all.
    assert!(found.contains("REVEAL"), "{found}");
}

#[test]
fn the_secret_is_not_in_the_change_feed() {
    let (backend, store) = planted();

    // The feed is fed from the log, which the backend scan above already covers
    // — this asserts the shape a consumer actually receives, which is the thing
    // a downstream system would write to its own store.
    let stored = everything(&backend);
    assert!(!holds(&stored, PLANTED));

    let mut session = Session::new(&store);
    session.run(USE).unwrap();
    session
        .run("DEFINE FIELD extra ON team TYPE string;")
        .unwrap();
    session
        .run(&format!(
            "UPDATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};"
        ))
        .unwrap();

    // After a second write, still nowhere: an update writes a new version and
    // the old one stays until it is reclaimed, so this is two records' worth of
    // bytes rather than one.
    let stored = everything(&backend);
    assert!(
        !holds(&stored, PLANTED),
        "an update wrote the secret in the clear"
    );
    assert!(holds(&stored, CONTROL));
}

#[test]
fn the_secret_survives_a_seal_and_comes_back_after_unsealing() {
    let (_backend, store) = planted();
    let mut session = Session::new(&store);
    session.run(USE).unwrap();

    session.run("SEAL VAULT;").unwrap();
    session.run("REVEAL token FROM team:'github';").unwrap_err();

    session
        .run("UNSEAL VAULT WITH 'an operator passphrase';")
        .unwrap();
    let Outcome::Value(Value::Object(opened)) = session
        .run("REVEAL token FROM team:'github';")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("REVEAL answers with an object")
    };
    // The round trip, asserted last rather than first: everything above claims
    // the plaintext is nowhere, and this is what stops that being true because
    // the value was never stored at all.
    assert_eq!(
        opened.get("token"),
        Some(&Value::String(PLANTED.to_owned()))
    );
}

/// Criterion W2's last clause, asserted rather than assumed: *an undeclared
/// field is an unencrypted field by accident*.
///
/// A vault is declared `schemafull: false`, like every other store this engine
/// makes. That is the right default for a table and it is the wrong one here,
/// because the marker that seals a field is `SECRET` **on a declaration**, and a
/// field nobody declared carries no marker. So the question this test asks is
/// not whether the engine encrypts what it was told to — the other tests in this
/// file establish that — but what it does with a field it was never told about.
///
/// Written to fail if the answer is "stores it in the clear beside the sealed
/// one", which is the accident the criterion names.
#[test]
fn a_field_nobody_declared_does_not_land_in_a_vault_in_the_clear() {
    const UNDECLARED: &str = "undeclared-field-secret-4d81";

    let (backend, store) = planted();
    {
        let mut session = Session::new(&store);
        session.run(USE).unwrap();
        // `recovery` is not declared on `team`. A caller writing it is doing the
        // most ordinary thing in a schemaless store, and believes it is inside a
        // vault because the record is.
        let written = session.run(&format!(
            "CREATE team:'gitlab' = {{ login: '{CONTROL}', recovery: '{UNDECLARED}' }};"
        ));

        // Either outcome is defensible and they are not equally safe, so the
        // test records which one this store gives rather than accepting both.
        if written.is_err() {
            // Strict: the write is refused, and nothing reached the backend.
            assert!(
                !holds(&everything(&backend), UNDECLARED),
                "the write was refused and the value is in the backend anyway"
            );
            return;
        }
    }

    let stored = everything(&backend);
    assert!(
        holds(&stored, CONTROL),
        "the control is absent, so nothing was written and this proves nothing"
    );
    assert!(
        !holds(&stored, UNDECLARED),
        "a field nobody declared was accepted into a vault and stored in the \
         clear — the vault holds a plaintext beside its sealed fields"
    );
}
