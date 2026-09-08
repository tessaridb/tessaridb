//! The restore drill: a vault comes back from a backup, and says what it took.
//!
//! # Why a test and not a runbook
//!
//! Readiness row R4 asks for a **rehearsed and timed** restore, *including what
//! unsealing it required*. Every part of that had been reasoned about and no
//! part of it had been done: the pieces existed — a backup writer, a bootstrap
//! path, a root record, an `UNSEAL` statement — and nobody had ever run them in
//! that order. Knowing the shape of a ceremony is not the same as having
//! performed it, and the difference is exactly where a missing step lives.
//!
//! # What this proves, and what it does not
//!
//! It proves a restored store holds a vault it cannot open, that the passphrase
//! and nothing else opens it, and that what comes back is what went in. It does
//! **not** produce an operational restore time: this is an in-process store on
//! a memory backend, so the numbers below are the cost of the *path*, not of a
//! real restore on real storage. The readiness checklist records it that way.
//!
//! The refusal in the middle is the load-bearing assertion. If a freshly
//! bootstrapped store answered `REVEAL`, the backup would be carrying a key
//! somebody could use, and every other claim this feature makes about backups
//! would be false.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use tessari_kv::{KeyRange, Keyspace, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";

/// An ordinary field's value, equally distinctive and **not** secret.
///
/// Its job is to fail the scan below if the scan is broken: a search that cannot
/// find a string written in the clear proves nothing by not finding one that was
/// sealed.
const CONTROL: &str = "ordinary-value-8c4d-not-a-secret";
const PASSPHRASE: &str = "an operator passphrase, held by a person";
const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Everything the backend holds, across every keyspace it has.
///
/// Enumerated rather than named, so a keyspace added later is covered without
/// anybody remembering to come back here.
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

/// How many entries one keyspace holds.
fn entries_in(backend: &Arc<dyn KvBackend>, keyspace: Keyspace) -> usize {
    let request = ScanRequest {
        keyspace,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    backend.scan(&request).unwrap().len()
}

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

#[test]
fn a_vault_survives_a_backup_and_opens_again_with_only_the_passphrase() {
    // The node that gets backed up.
    let (_source_backend, source) = store();
    {
        let mut session = Session::new(&source);
        session
            .run(&format!(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;
                 DEFINE DATABASE work; USE DATABASE work;
                 UNSEAL VAULT WITH '{PASSPHRASE}';
                 DEFINE VAULT team;
                 DEFINE FIELD login ON team TYPE string;
                 DEFINE FIELD token ON team TYPE string SECRET;
                 DEFINE INDEX by_login ON team FIELDS login;
                 CREATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};"
            ))
            .unwrap();
    }

    let backup_started = Instant::now();
    let mut artifact = Vec::new();
    let written = tessari_backup::write(&source, &mut artifact).unwrap();
    let backup_took = backup_started.elapsed();
    assert!(written.records > 0, "a backup that wrote nothing");

    // A restored node is a *new process*: nothing has been unsealed, which is
    // the state an operator actually finds after a restore rather than the one
    // a test would arrive at by reusing the source store.
    let (restored_backend, restored) = store();
    let restore_started = Instant::now();
    let brought_up = tessari_backup::bootstrap(&restored, &mut artifact.as_slice()).unwrap();
    let restore_took = restore_started.elapsed();
    assert!(!brought_up.truncated);
    assert!(brought_up.records > 0, "a restore that applied nothing");

    let mut session = Session::new(&restored);
    session.run(USING).unwrap();

    // The control first: the vault and its record are here. Without this, the
    // refusal below could mean the backup carried nothing at all.
    session.run("INFO FOR VAULT team;").unwrap();

    // Sealed. This is the assertion the whole drill exists for.
    session
        .run("REVEAL token FROM team:'github';")
        .expect_err("a restored store opened a vault before anybody unsealed it");

    // And the passphrase has to be the right one — a restore that accepted any
    // passphrase would be carrying the key rather than the wrapper.
    session
        .run("UNSEAL VAULT WITH 'not the passphrase';")
        .expect_err("a restored store unsealed with the wrong passphrase");

    // What unsealing required: the passphrase, and nothing else. No key file,
    // no shard ceremony, no second artifact — the root record travelled in the
    // backup, and it is safe to carry there because it is the passphrase that
    // turns it into a key.
    let unseal_started = Instant::now();
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();
    let unseal_took = unseal_started.elapsed();

    let reveal_started = Instant::now();
    let opened = match session
        .run("REVEAL token FROM team:'github';")
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    };
    let reveal_took = reveal_started.elapsed();

    let Value::Object(fields) = opened else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned())),
        "the restored secret is not the one that was written"
    );

    // Row 34 of the negative matrix: what the follower's own store holds.
    //
    // This is not the same question the exfiltration tests answered about the
    // leader, and it is not redundant with them, because a follower does not
    // copy bytes — it replays log records through the apply path, which drives
    // the same index writer the leader's writes drive. `by_login` exists on this
    // vault precisely so that path runs here. The claim that a follower's index
    // holds ciphertext rests on the payload being ciphertext before it reaches
    // the writer, and that is the step nothing had ever observed.
    let held = everything(&restored_backend);
    assert!(
        !holds(&held, PLANTED),
        "the restored store holds the secret in the clear"
    );
    assert!(
        holds(&held, CONTROL),
        "the scan found neither the secret nor the control, so it is looking at \
         an empty store and proves nothing"
    );

    // And the index keyspace is not empty, which the control above does not
    // establish: `login` is an ordinary field, so its value is in the record
    // whether or not an index was ever written. Without this line the scan
    // covers the index path in principle and possibly over nothing.
    assert!(
        entries_in(&restored_backend, Keyspace::INDEX) > 0,
        "the follower wrote no index entries, so scanning its index keyspace \
         proves nothing about what an index writer does with a sealed record"
    );

    // The numbers R4 asks for. Printed rather than asserted: a threshold here
    // would be a guess about hardware, and the row wants a measurement.
    println!(
        "RESTORE_DRILL records={} backup={:?} restore={:?} unseal={:?} reveal={:?}",
        brought_up.records, backup_took, restore_took, unseal_took, reveal_took
    );
}
