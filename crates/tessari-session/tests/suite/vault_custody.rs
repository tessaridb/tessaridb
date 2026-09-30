//! A vault may carry its own passphrase (ADR-0093): its key is wrapped under a
//! key derived from that passphrase rather than under the store's master key,
//! so the store's passphrase — and store-wide authority — open nothing in it.
//!
//! Each test reads a secret back, or is refused reading it, rather than
//! checking that a statement ran: a vault whose own passphrase was accepted and
//! then ignored in favour of the master key would pass every test that did not.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const STORE_PASSPHRASE: &str = "the store passphrase";
const TEAM_PASSPHRASE: &str = "the team passphrase";
const PLANTED: &str = "team-secret-4c1d";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A store with a store-custody vault `shared` and an own-custody vault `team`,
/// each holding one secret, with the store unsealed.
fn two_custodies(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;
             DEFINE DATABASE work; DEFINE DATABASE other; USE DATABASE work;
             UNSEAL VAULT WITH '{STORE_PASSPHRASE}';
             DEFINE VAULT shared;
             DEFINE FIELD token ON shared TYPE string SECRET;
             CREATE shared:'github' = {{ token: 'shared-secret' }};
             DEFINE VAULT team PASSPHRASE '{TEAM_PASSPHRASE}';
             DEFINE FIELD token ON team TYPE string SECRET;
             CREATE team:'github' = {{ token: '{PLANTED}' }};"
        ))
        .unwrap();
    session
}

fn reveal(session: &mut Session<'_>, vault: &str) -> Result<Value, Error> {
    match session
        .run(&format!("REVEAL token FROM {vault}:'github';"))?
        .pop()
        .unwrap()
    {
        Outcome::Value(Value::Object(mut fields)) => Ok(fields.remove("token").unwrap()),
        other => panic!("expected the revealed fields, got {other:?}"),
    }
}

fn is_sealed(refused: &Error) -> bool {
    matches!(
        refused,
        Error::Store(tessari_storage::Error::Vault(tessari_vault::Error::Sealed))
    )
}

fn seal_of(session: &mut Session<'_>, vault: &str) -> BTreeMap<String, Value> {
    match session
        .run(&format!("INFO FOR SEAL OF {vault};"))
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Value(Value::Object(fields)) => fields,
        other => panic!("expected an object, got {other:?}"),
    }
}

#[test]
fn a_vault_with_its_own_passphrase_is_not_opened_by_the_stores() {
    let store = store();
    let mut session = two_custodies(&store);
    // Declaring it unsealed it, for one period.
    assert_eq!(reveal(&mut session, "team").unwrap(), Value::from(PLANTED));

    session.run("SEAL VAULT team;").unwrap();
    let refused = reveal(&mut session, "team").expect_err("a sealed own vault opened");
    assert!(is_sealed(&refused), "refused, but not as sealed: {refused}");
    // The store is unsealed throughout, and its own vault still opens.
    assert_eq!(
        reveal(&mut session, "shared").unwrap(),
        Value::from("shared-secret")
    );

    session
        .run(&format!("UNSEAL VAULT team WITH '{TEAM_PASSPHRASE}';"))
        .unwrap();
    assert_eq!(reveal(&mut session, "team").unwrap(), Value::from(PLANTED));
}

#[test]
fn a_vault_with_its_own_passphrase_needs_no_store_passphrase_at_all() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;
             DEFINE DATABASE work; USE DATABASE work;
             DEFINE VAULT team PASSPHRASE '{TEAM_PASSPHRASE}';
             DEFINE FIELD token ON team TYPE string SECRET;
             CREATE team:'github' = {{ token: '{PLANTED}' }};"
        ))
        .unwrap();
    assert_eq!(reveal(&mut session, "team").unwrap(), Value::from(PLANTED));
    let store_state = match session.run("INFO FOR SEAL;").unwrap().pop().unwrap() {
        Outcome::Value(Value::Object(mut fields)) => fields.remove("state").unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(store_state, Value::from("uninitialised"));
}

#[test]
fn the_stores_passphrase_does_not_unseal_an_own_vault_and_its_own_opens_only_it() {
    let store = store();
    let mut session = two_custodies(&store);
    session.run("SEAL VAULT team;").unwrap();
    session.run("SEAL VAULT;").unwrap();

    let refused = session
        .run(&format!("UNSEAL VAULT team WITH '{STORE_PASSPHRASE}';"))
        .expect_err("the store's passphrase unsealed an own vault");
    assert!(
        matches!(
            refused,
            Error::Store(tessari_storage::Error::Vault(
                tessari_vault::Error::WrongKey
            ))
        ),
        "refused, but not as the wrong key: {refused}"
    );
    assert!(!refused.to_string().contains(STORE_PASSPHRASE), "{refused}");

    session
        .run(&format!("UNSEAL VAULT team WITH '{TEAM_PASSPHRASE}';"))
        .unwrap();
    assert_eq!(reveal(&mut session, "team").unwrap(), Value::from(PLANTED));
    let refused = reveal(&mut session, "shared").expect_err("an own passphrase opened the store");
    assert!(is_sealed(&refused), "{refused}");
}

#[test]
fn a_vault_under_the_stores_passphrase_is_not_unsealed_by_naming_it() {
    let store = store();
    let mut session = two_custodies(&store);
    session.run("SEAL VAULT;").unwrap();
    for statement in [
        format!("UNSEAL VAULT shared WITH '{STORE_PASSPHRASE}';"),
        "SEAL VAULT shared;".to_owned(),
        format!("CHANGE VAULT shared PASSPHRASE FROM '{STORE_PASSPHRASE}' TO 'x';"),
    ] {
        let refused = session.run(&statement).expect_err(&statement);
        assert!(
            matches!(refused, Error::VaultUsesStorePassphrase { .. }),
            "{statement} was refused, but not for its custody: {refused}"
        );
    }
    // Naming it opened nothing on the way to the refusal.
    assert!(is_sealed(&reveal(&mut session, "shared").unwrap_err()));
}

#[test]
fn the_seal_of_a_vault_names_its_custody_and_its_own_state() {
    let store = store();
    let mut session = two_custodies(&store);
    let team = seal_of(&mut session, "team");
    assert_eq!(team.get("custody"), Some(&Value::from("own")));
    assert_eq!(team.get("state"), Some(&Value::from("unsealed")));
    assert!(
        matches!(team.get("seals_at"), Some(Value::Datetime(_))),
        "{team:?}"
    );

    session.run("SEAL VAULT team;").unwrap();
    assert_eq!(
        seal_of(&mut session, "team").get("state"),
        Some(&Value::from("sealed"))
    );
    // A store-custody vault answers with the store's state.
    let shared = seal_of(&mut session, "shared");
    assert_eq!(shared.get("custody"), Some(&Value::from("store")));
    assert_eq!(shared.get("state"), Some(&Value::from("unsealed")));
    session.run("SEAL VAULT;").unwrap();
    assert_eq!(
        seal_of(&mut session, "shared").get("state"),
        Some(&Value::from("sealed"))
    );
}

#[test]
fn a_changed_vault_passphrase_opens_its_secrets_and_the_old_one_does_not() {
    let store = store();
    let mut session = two_custodies(&store);
    session
        .run(&format!(
            "CHANGE VAULT team PASSPHRASE FROM '{TEAM_PASSPHRASE}' TO 'the next team passphrase';"
        ))
        .unwrap();
    session.run("SEAL VAULT team;").unwrap();
    let refused = session
        .run(&format!("UNSEAL VAULT team WITH '{TEAM_PASSPHRASE}';"))
        .expect_err("the old vault passphrase still unseals");
    assert!(
        matches!(
            refused,
            Error::Store(tessari_storage::Error::Vault(
                tessari_vault::Error::WrongKey
            ))
        ),
        "{refused}"
    );
    session
        .run("UNSEAL VAULT team WITH 'the next team passphrase';")
        .unwrap();
    assert_eq!(reveal(&mut session, "team").unwrap(), Value::from(PLANTED));
}

#[test]
fn an_own_vault_unsealed_past_its_period_is_sealed() {
    let store = store();
    let mut session = two_custodies(&store);
    session.run("SEAL VAULT team;").unwrap();
    store.vault().last_for(std::time::Duration::ZERO);
    session
        .run(&format!("UNSEAL VAULT team WITH '{TEAM_PASSPHRASE}';"))
        .unwrap();
    assert!(is_sealed(&reveal(&mut session, "team").unwrap_err()));
    assert_eq!(
        seal_of(&mut session, "team").get("state"),
        Some(&Value::from("sealed"))
    );
}

#[test]
fn an_own_vault_guessed_three_times_wrong_waits_and_the_store_does_not() {
    let store = store();
    let mut session = two_custodies(&store);
    session.run("SEAL VAULT team;").unwrap();
    session.run("SEAL VAULT;").unwrap();
    for guess in ["one", "two", "three"] {
        let refused = session
            .run(&format!("UNSEAL VAULT team WITH '{guess}';"))
            .expect_err("a wrong vault passphrase unsealed");
        assert!(!matches!(refused, Error::PassphraseThrottled));
    }
    let refused = session
        .run(&format!("UNSEAL VAULT team WITH '{TEAM_PASSPHRASE}';"))
        .expect_err("a fourth attempt was tried at once");
    assert!(matches!(refused, Error::PassphraseThrottled), "{refused}");
    // The store's passphrase is a different thing being guessed.
    session
        .run(&format!("UNSEAL VAULT WITH '{STORE_PASSPHRASE}';"))
        .unwrap();
}

#[test]
fn a_reader_of_the_vault_may_open_it_and_only_a_manager_may_change_its_passphrase() {
    let store = store();
    let mut anonymous = two_custodies(&store);
    anonymous
        .run("DEFINE USER root ROLE owner PASSWORD 'root-password';")
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", "root-password").unwrap();
    root.run(
        "DEFINE USER ada ON prod.work ROLE viewer PASSWORD 'ada-password';
         USE NAMESPACE prod; USE DATABASE work; SEAL VAULT team;",
    )
    .unwrap();

    let mut ada = Session::new(&store);
    ada.sign_in("ada", "ada-password").unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE work;").unwrap();
    ada.run(&format!("UNSEAL VAULT team WITH '{TEAM_PASSPHRASE}';"))
        .unwrap();
    assert_eq!(reveal(&mut ada, "team").unwrap(), Value::from(PLANTED));
    let refused = ada
        .run(&format!(
            "CHANGE VAULT team PASSPHRASE FROM '{TEAM_PASSPHRASE}' TO 'mine now';"
        ))
        .expect_err("a viewer rewrote the vault's passphrase");
    assert!(
        matches!(refused, Error::RoleForbids { needs, .. } if needs == "manage"),
        "{refused}"
    );
    ada.run("SEAL VAULT team;").unwrap();
    assert!(is_sealed(&reveal(&mut ada, "team").unwrap_err()));
}

#[test]
fn info_for_vault_names_its_custody() {
    let store = store();
    let mut session = two_custodies(&store);
    for (vault, custody) in [("team", "own"), ("shared", "store")] {
        let report = match session
            .run(&format!("INFO FOR VAULT {vault};"))
            .unwrap()
            .pop()
            .unwrap()
        {
            Outcome::Value(Value::Object(fields)) => fields,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            report.get("custody"),
            Some(&Value::from(custody)),
            "{vault}"
        );
    }
}
