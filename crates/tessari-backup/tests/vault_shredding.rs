//! What `DROP VAULT` destroys, and what it does not.
//!
//! # The claim this file measures
//!
//! Dropping a vault destroys its key, and the documentation drew a strong
//! conclusion from that: every copy of the vault's records, in every backup,
//! snapshot and replica that will ever be restored, becomes ciphertext under a
//! key that exists nowhere.
//!
//! The conclusion does not follow, and the reason is visible from the other
//! direction in `vault_restore.rs`: a vault's wrapped key lives in its **table
//! definition**, the definition travels in the **log**, and a backup *is* the
//! log. That is exactly why a follower ends up with an openable vault — which is
//! a feature there and the whole problem here.
//!
//! So a backup taken **before** the drop restores a vault that opens. This file
//! asserts that, deliberately, because it is the true behaviour and the reason
//! the documentation now says something narrower.
//!
//! # What is actually destroyed
//!
//! The key in **this store**, permanently. Every copy made **after** the drop is
//! unopenable, and so is the store itself. Crypto-shredding is real here; its
//! boundary is the moment of the drop, not the whole history of the data.
//!
//! The operational consequence belongs with the feature and not in a comment:
//! destroying a secret means expiring the backups that predate the destruction.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";
const PASSPHRASE: &str = "an operator passphrase, held by a person";
const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A store holding one sealed secret, and the backup taken before anything else.
fn with_a_secret_and_its_backup() -> (Store, Vec<u8>) {
    let store = store();
    {
        let mut session = Session::new(&store);
        session
            .run(&format!(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;
                 DEFINE DATABASE work; USE DATABASE work;
                 UNSEAL VAULT WITH '{PASSPHRASE}';
                 DEFINE VAULT team;
                 DEFINE FIELD token ON team TYPE string SECRET;
                 CREATE team:'github' = {{ token: '{PLANTED}' }};"
            ))
            .unwrap();
    }
    let mut artifact = Vec::new();
    tessari_backup::write(&store, &mut artifact).unwrap();
    (store, artifact)
}

#[test]
fn a_backup_taken_before_the_drop_restores_a_vault_that_opens() {
    let (source, before_the_drop) = with_a_secret_and_its_backup();

    {
        let mut session = Session::new(&source);
        session.run(USING).unwrap();
        session.run("DROP VAULT team;").unwrap();
        // Gone here, and gone for good: the key this store held is destroyed.
        session
            .run("REVEAL token FROM team:'github';")
            .expect_err("the dropped vault answered a read");
    }

    // And back, from a file somebody kept. The passphrase is all it takes,
    // because the wrapped key travelled in the log along with everything else.
    let restored = store();
    tessari_backup::bootstrap(&restored, &mut before_the_drop.as_slice()).unwrap();
    let mut session = Session::new(&restored);
    session.run(USING).unwrap();
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();
    let opened = match session
        .run("REVEAL token FROM team:'github';")
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    };
    let Value::Object(fields) = opened else {
        panic!("REVEAL answers with an object")
    };
    assert_eq!(
        fields.get("token"),
        Some(&Value::String(PLANTED.to_owned())),
        "the pre-drop backup no longer opens — if this is now the behaviour, the \
         documentation and criterion K3 both need revisiting, because they were \
         written against the opposite"
    );
}

#[test]
fn a_backup_taken_after_the_drop_carries_nothing_that_opens() {
    let (source, _before) = with_a_secret_and_its_backup();
    {
        let mut session = Session::new(&source);
        session.run(USING).unwrap();
        session.run("DROP VAULT team;").unwrap();
    }

    // The half of crypto-shredding that does hold, and the reason the claim is
    // narrowed rather than withdrawn. Everything from here on is unopenable,
    // including this artifact, because the key it would need is destroyed.
    let mut after_the_drop = Vec::new();
    tessari_backup::write(&source, &mut after_the_drop).unwrap();

    let restored = store();
    tessari_backup::bootstrap(&restored, &mut after_the_drop.as_slice()).unwrap();
    let mut session = Session::new(&restored);
    session.run(USING).unwrap();
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();
    session
        .run("REVEAL token FROM team:'github';")
        .expect_err("a backup taken after the drop restored an openable vault");
}
