//! Reach and open are two powers, and neither passes by the other's route.
//!
//! # Why this needs its own file
//!
//! Every other test of a vault asserts that something is refused. These assert
//! **which refusal fired**, and that is the whole content: a store where the
//! permission check happened to also cover the sealed case, or where an unsealed
//! store happened to also skip the grant, would pass a test that only checked
//! for an error. The two mechanisms have to be shown failing separately.
//!
//! - **Reach** — may this user address this vault? Answered by a grant,
//!   enforced at the door, revoked by editing a row.
//! - **Open** — may this party turn these bytes into a secret? Answered by
//!   holding the passphrase, enforced at decryption, revoked by nothing you can
//!   edit.
//!
//! An owner with every authority the store offers and a sealed keyring is
//! refused at decryption. A caller whose process holds the master key and who
//! was never granted the vault is refused at the door. Both refusals are real,
//! and each is asserted by the words the *other* one cannot produce.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";
const PLANTED: &str = "correct-horse-battery-staple-9f2b";

const USING: &str = "USE NAMESPACE prod; USE DATABASE work;";

/// A store with a vault, a record, an owner and an editor granted elsewhere.
///
/// The editor holds a grant on `notes` and none on `team`, deliberately: a user
/// with no grants at all passes the reach check vacuously — the loop over what a
/// statement names finds nothing to refuse — so a test built on one would assert
/// nothing at all.
fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    {
        let mut session = Session::new(&store);
        session
            .run(&format!(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;
                 DEFINE DATABASE work; USE DATABASE work;
                 UNSEAL VAULT WITH 'an operator passphrase';
                 DEFINE VAULT team;
                 DEFINE FIELD token ON team TYPE string SECRET;
                 CREATE team:'github' = {{ token: '{PLANTED}' }};
                 DEFINE TABLE notes SCHEMALESS;
                 DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
            ))
            .unwrap();
        let mut root = Session::new(&store);
        root.sign_in("root", PASSWORD).unwrap();
        root.run(&format!(
            "{USING}
             DEFINE USER ada ON prod.work ROLE editor PASSWORD '{PASSWORD}';
             GRANT read, write ON notes TO ada;"
        ))
        .unwrap();
    }
    store
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session.run(USING).unwrap();
    session
}

#[test]
fn every_grant_and_no_key_is_refused_at_decryption() {
    let store = store();
    let mut root = signed_in(&store, "root");

    // The control first: with the keyring open, this exact user reads the
    // secret. So the refusal below is about the key and not about the user.
    let opened = root.run("REVEAL token FROM team:'github';").unwrap();
    assert!(format!("{opened:?}").contains(PLANTED));

    root.run("SEAL VAULT;").unwrap();

    let refused = root
        .run("REVEAL token FROM team:'github';")
        .expect_err("a sealed store served a secret to an owner")
        .to_string();

    // The words a permission refusal cannot produce. An owner holds every
    // authority this store offers, so nothing about a grant is what stopped
    // this — and if the message said otherwise, the two mechanisms would have
    // collapsed into one.
    assert!(refused.contains("sealed"), "{refused}");
    assert!(
        !refused.contains("permission") && !refused.contains("not allowed"),
        "the refusal reads as a permission failure: {refused}",
    );
    assert!(!refused.contains(PLANTED), "the refusal quoted the secret");
}

#[test]
fn the_key_without_the_grant_is_refused_at_the_door() {
    let store = store();
    // The keyring stays open for the whole process — this is the case where the
    // party genuinely can decrypt, and is stopped before reaching anything to
    // decrypt.
    let mut ada = signed_in(&store, "ada");

    let refused = ada
        .run("REVEAL token FROM team:'github';")
        .expect_err("an ungranted user read a vault")
        .to_string();

    // The words a decryption refusal cannot produce. The store is unsealed, so
    // nothing about a key stopped this.
    assert!(refused.contains("team"), "{refused}");
    assert!(
        !refused.contains("sealed"),
        "the refusal reads as a sealed store: {refused}",
    );
    assert!(!refused.contains(PLANTED), "the refusal quoted the secret");
}

#[test]
fn the_grant_alone_does_not_carry_the_key() {
    let store = store();
    {
        let mut root = signed_in(&store, "root");
        root.run("GRANT read, write ON team TO ada;").unwrap();
        root.run("SEAL VAULT;").unwrap();
    }

    // Ada now holds the grant the previous test said she lacked, and the store
    // is sealed. Granting reach did not grant opening: the two are separate
    // rows in the ledger and separate mechanisms in the code.
    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("REVEAL token FROM team:'github';")
        .expect_err("a granted user opened a sealed vault")
        .to_string();
    assert!(refused.contains("sealed"), "{refused}");
}

#[test]
fn the_key_alone_does_not_carry_the_grant() {
    let store = store();
    {
        // Sealed and unsealed again, so the key in this process was acquired by
        // an act somebody took rather than left over from the fixture. Unsealing
        // is store-wide and is the operator's, not any one user's.
        let mut root = signed_in(&store, "root");
        root.run("SEAL VAULT; UNSEAL VAULT WITH 'an operator passphrase';")
            .unwrap();
        // The control: the keyring really is open now.
        let opened = root.run("REVEAL token FROM team:'github';").unwrap();
        assert!(format!("{opened:?}").contains(PLANTED));
    }

    // Ada's process can decrypt every vault in this store and she still cannot
    // address this one. The unsealing changed nothing about who may reach what.
    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("REVEAL token FROM team:'github';")
        .expect_err("an ungranted user read a vault on an unsealed store")
        .to_string();
    assert!(refused.contains("team"), "{refused}");
    assert!(!refused.contains("sealed"), "{refused}");
}
