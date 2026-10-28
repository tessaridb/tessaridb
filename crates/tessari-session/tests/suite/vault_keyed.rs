//! A vault may be keyed by a key its caller presents with each request (ADR-0136): its key is wrapped under a key
//! derived from the presented one, nothing in the process holds it between requests, and no passphrase, unseal or
//! period applies.
//!
//! Each test reads a secret back, or is refused reading it, rather than checking that a statement ran.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, PresentedKey, RunNow, Session};
use tessari_storage::{Now, Store, presenting};
use tessari_types::Value;

const STORE_PASSPHRASE: &str = "the store passphrase";
const KEY: [u8; 32] = [0x5a; 32];
const OTHER_KEY: [u8; 32] = [0x6b; 32];
const PLANTED: &str = "keyed-secret-91aa";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

fn key_for(vault: &str, key: [u8; 32]) -> PresentedKey {
    PresentedKey {
        namespace: "prod".to_owned(),
        database: "work".to_owned(),
        vault: vault.to_owned(),
        key,
    }
}

/// Run `source` with `keys` presented, as a request carrying them does.
fn run_presenting(
    session: &mut Session<'_>,
    keys: &[PresentedKey],
    source: &str,
) -> Result<Vec<Outcome>, Error> {
    let presented = session.presented_keys(keys).now()?;
    presenting(presented, Box::pin(session.run(source))).now()
}

/// A store with a keyed vault `team` holding one secret, written in a request that presented its key.
fn keyed(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;
             DEFINE DATABASE work; USE DATABASE work;",
        )
        .unwrap();
    session
        .declare_keyed_vault("prod", "work", "team", KEY)
        .now()
        .unwrap();
    session
        .run_now("DEFINE FIELD token ON team TYPE string SECRET;")
        .unwrap();
    run_presenting(
        &mut session,
        &[key_for("team", KEY)],
        &format!("CREATE team:'github' = {{ token: '{PLANTED}' }};"),
    )
    .unwrap();
    session
}

fn revealed(outcomes: Vec<Outcome>) -> Value {
    match outcomes.into_iter().last().unwrap() {
        Outcome::Value(Value::Object(mut fields)) => fields.remove("token").unwrap(),
        other => panic!("expected the revealed fields, got {other:?}"),
    }
}

fn is_wrong_key(refused: &Error) -> bool {
    matches!(
        refused,
        Error::Store(tessari_storage::Error::Vault(
            tessari_vault::Error::WrongKey
        ))
    )
}

const REVEAL: &str = "REVEAL token FROM team:'github';";

#[test]
fn a_keyed_vault_opens_in_a_request_that_presents_its_key_and_in_no_other() {
    let store = store();
    let mut session = keyed(&store);
    let opened = run_presenting(&mut session, &[key_for("team", KEY)], REVEAL).unwrap();
    assert_eq!(revealed(opened), Value::from(PLANTED));

    // The next request presents nothing, and nothing was kept from the last one.
    let refused = session
        .run_now(REVEAL)
        .expect_err("a keyed vault opened without its key");
    assert!(
        matches!(
            refused,
            Error::Store(tessari_storage::Error::Vault(tessari_vault::Error::Sealed))
        ),
        "refused, but not as sealed: {refused}"
    );
}

#[test]
fn a_wrong_key_is_refused() {
    let store = store();
    let mut session = keyed(&store);
    let refused = run_presenting(&mut session, &[key_for("team", OTHER_KEY)], REVEAL)
        .expect_err("a wrong key opened the vault");
    assert!(
        is_wrong_key(&refused),
        "refused, but not as a wrong key: {refused}"
    );
}

#[test]
fn the_stores_passphrase_opens_nothing_in_a_keyed_vault() {
    let store = store();
    let mut session = keyed(&store);
    session
        .run_now(&format!("UNSEAL VAULT WITH '{STORE_PASSPHRASE}';"))
        .unwrap();
    assert!(
        session.run_now(REVEAL).is_err(),
        "the store's passphrase opened a keyed vault"
    );
}

#[test]
fn passphrase_acts_on_a_keyed_vault_are_refused_by_name() {
    let store = store();
    let mut session = keyed(&store);
    for act in [
        "UNSEAL VAULT team WITH 'anything';",
        "SEAL VAULT team;",
        "CHANGE VAULT team PASSPHRASE FROM 'a' TO 'b';",
    ] {
        let refused = session.run_now(act).expect_err(act);
        assert!(
            matches!(refused, Error::VaultOpensWithAKey { .. }),
            "{act} refused, but not as a keyed vault: {refused}"
        );
    }
}

#[test]
fn a_key_presented_for_a_vault_that_is_not_keyed_is_refused() {
    let store = store();
    let mut session = keyed(&store);
    session
        .run_now(
            "DEFINE VAULT own PASSPHRASE 'own pass';
             DEFINE FIELD token ON own TYPE string SECRET;",
        )
        .unwrap();
    let refused = run_presenting(&mut session, &[key_for("own", KEY)], "RETURN 1;")
        .expect_err("a key was accepted for an own-custody vault");
    assert!(
        matches!(refused, Error::VaultNotKeyed { .. }),
        "refused, but not as a vault that is not keyed: {refused}"
    );
}

#[test]
fn a_changed_key_opens_the_same_secrets_and_the_old_one_opens_nothing() {
    let store = store();
    let mut session = keyed(&store);
    session
        .change_vault_key("prod", "work", "team", KEY, OTHER_KEY)
        .now()
        .unwrap();
    let opened = run_presenting(&mut session, &[key_for("team", OTHER_KEY)], REVEAL).unwrap();
    assert_eq!(revealed(opened), Value::from(PLANTED));
    let refused = run_presenting(&mut session, &[key_for("team", KEY)], REVEAL)
        .expect_err("the old key still opened the vault");
    assert!(
        is_wrong_key(&refused),
        "refused, but not as a wrong key: {refused}"
    );

    let refused = session
        .change_vault_key("prod", "work", "team", KEY, OTHER_KEY)
        .now()
        .expect_err("a change proven with the old key was accepted");
    assert!(
        is_wrong_key(&refused),
        "refused, but not as a wrong key: {refused}"
    );
}

#[test]
fn info_answers_key_custody_and_sealed() {
    let store = store();
    let mut session = keyed(&store);
    let Outcome::Value(Value::Object(report)) = session
        .run_now("INFO FOR SEAL OF team;")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("expected an object");
    };
    assert_eq!(report.get("custody"), Some(&Value::from("key")));
    assert_eq!(report.get("state"), Some(&Value::from("sealed")));
}
