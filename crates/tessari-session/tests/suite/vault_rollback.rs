//! Restoring an older sealed payload under a record that has since been rotated.
//!
//! # What this file is for
//!
//! Row 33 of the vault's negative-test matrix — *rolled back* — is the one row
//! nothing defends, and it had no test. A row asserted to be undefended without
//! a test is two claims wearing one coat: that the attack works, and that it
//! works for the reason somebody wrote down. Only the first is observable, and
//! it is the one that decides whether the second matters.
//!
//! So this file demonstrates the attack rather than asserting its absence. What
//! it pins is a **published boundary**, in the same way `vault_shredding.rs`
//! pins where the deletion claim stops: the test that fails when the store
//! quietly becomes stronger is as useful as the one that fails when it becomes
//! weaker, because a boundary nobody re-measures is a boundary that drifts.
//!
//! # And it settles the fix that was proposed for it
//!
//! The recorded fix was to bind a version into the envelope's associated data.
//! The second test here is why that would buy nothing: the attacker restores
//! the record's stored bytes, and a version kept in those bytes is restored
//! along with them, so both sides of the comparison move together and every
//! check still passes. Anti-rollback needs monotonic state the attacker cannot
//! reach, and an embedded engine whose only durable state is the backend under
//! attack has none to offer.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{
    KeyRange, Keyspace, KvBackend, MemoryBackend, ScanDirection, ScanRequest, WriteBatch,
};
use tessari_session::Session;
use tessari_storage::Store;

const ROTATED_OUT: &str = "the-old-token-1a2b3c";
const ROTATED_IN: &str = "the-new-token-4d5e6f";

const SETUP: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
UNSEAL VAULT WITH 'an operator passphrase';
DEFINE VAULT team;
DEFINE FIELD token ON team TYPE string SECRET;
";

const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

/// Every pair the record store holds, which is what a backup of it would carry.
fn records(backend: &Arc<dyn KvBackend>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let request = ScanRequest {
        keyspace: Keyspace::DATA,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    backend
        .scan(&request)
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key.as_slice().to_vec(), value.as_slice().to_vec()))
        .collect()
}

/// Put the record store back exactly as it stood, the way an attacker with
/// write access to the backend would.
///
/// Both halves are needed and the second is the one that does the work: writing
/// the old pairs back leaves the newer versions in place beside them, and a read
/// resolves to the newest. Removing what appeared since is what makes the old
/// bytes the answer again.
fn restore(backend: &Arc<dyn KvBackend>, taken: &[(Vec<u8>, Vec<u8>)]) {
    let mut batch = WriteBatch::new();
    for (key, _) in records(backend) {
        if !taken.iter().any(|(was, _)| *was == key) {
            batch = batch.delete(Keyspace::DATA, key.into());
        }
    }
    for (key, value) in taken {
        batch = batch.put(Keyspace::DATA, key.clone().into(), value.clone().into());
    }
    backend.apply(batch).unwrap();
}

fn opened(session: &mut Session<'_>) -> String {
    format!(
        "{:?}",
        session.run("REVEAL token FROM team:'github';").unwrap()
    )
}

#[test]
fn an_older_sealed_payload_restored_under_a_rotated_record_opens() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = Session::new(&store);
    session.run(SETUP).unwrap();
    session
        .run(&format!(
            "CREATE team:'github' = {{ token: '{ROTATED_OUT}' }};"
        ))
        .unwrap();

    // The backup an operator would take, and the rotation they would perform
    // afterwards — the ordinary sequence, not a contrived one. The rotation is
    // written whole because a vault's record is; `SET` is refused, and
    // `vault_language.rs` is where that refusal is pinned.
    let backup = records(&backend);
    assert!(!backup.is_empty(), "nothing was captured to restore");
    session
        .run(&format!(
            "UPDATE team:'github' = {{ token: '{ROTATED_IN}' }};"
        ))
        .unwrap();

    // The control: after the rotation the store answers with the new secret. A
    // test that skipped this would pass if the update had silently done nothing.
    let after = opened(&mut session);
    assert!(after.contains(ROTATED_IN), "{after}");
    assert!(!after.contains(ROTATED_OUT), "{after}");

    restore(&backend, &backup);

    // And here is the boundary, demonstrated. The envelope binds the table, the
    // record and the field, and nothing that changes between two writes of the
    // same field — so the restored ciphertext satisfies every check the store
    // makes, and the rotated-out secret is live again with nothing anywhere in
    // an error state.
    let mut reader = Session::new(&store);
    reader.run(USING).unwrap();
    let rolled = opened(&mut reader);
    assert!(
        rolled.contains(ROTATED_OUT),
        "the rollback did not take effect, so this test proves nothing: {rolled}",
    );
    assert!(!rolled.contains(ROTATED_IN), "{rolled}");
}

#[test]
fn a_version_kept_in_the_record_would_be_restored_with_it() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = Session::new(&store);
    session.run(SETUP).unwrap();
    // Sealed like the token, because a vault answers no `SELECT` — and because
    // that is where a version bound into the associated data would have to
    // live: on the record, readable by whoever opens it.
    session
        .run("DEFINE FIELD seal_version ON team TYPE string SECRET;")
        .unwrap();

    session
        .run(&format!(
            "CREATE team:'github' = {{ token: '{ROTATED_OUT}', seal_version: 'v1' }};"
        ))
        .unwrap();
    let backup = records(&backend);
    session
        .run(&format!(
            "UPDATE team:'github' = {{ token: '{ROTATED_IN}', seal_version: 'v2' }};"
        ))
        .unwrap();

    // The control: the rotation moved both, so the two really are written
    // together and the assertion below is about the restore rather than about
    // an update that never happened.
    let current = format!(
        "{:?}",
        session
            .run("REVEAL token, seal_version FROM team:'github';")
            .unwrap()
    );
    assert!(
        current.contains("v2") && current.contains(ROTATED_IN),
        "{current}"
    );

    restore(&backend, &backup);

    let mut reader = Session::new(&store);
    reader.run(USING).unwrap();
    let rolled = format!(
        "{:?}",
        reader
            .run("REVEAL token, seal_version FROM team:'github';")
            .unwrap()
    );

    // Both halves moved back together, which is the whole finding: a version
    // the opening side reads from the record is a version the attacker restores
    // along with the ciphertext it was meant to police. Binding it into the
    // associated data would make both sides of the comparison agree — on the
    // old write.
    assert!(
        rolled.contains("v1") && rolled.contains(ROTATED_OUT),
        "the version did not roll back with the ciphertext, which would make the \
         proposed fix workable after all — re-open Q-421: {rolled}",
    );
    assert!(!rolled.contains("v2"), "{rolled}");
}
