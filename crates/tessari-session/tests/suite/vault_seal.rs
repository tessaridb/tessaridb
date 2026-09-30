//! The vault's operational surface (ADR-0092): whether the store is sealed and
//! until when, an unseal that lasts a period, and a passphrase that can change.
//!
//! Each answer is asserted by its content, not by the absence of an error: a
//! status that always answered `sealed`, or a period nobody applied, would pass
//! a test that only checked that the statement ran.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSPHRASE: &str = "an operator passphrase";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn answer(session: &mut Session<'_>, statement: &str) -> std::collections::BTreeMap<String, Value> {
    match session.run(statement).unwrap().pop().unwrap() {
        Outcome::Value(Value::Object(fields)) => fields,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn state(session: &mut Session<'_>) -> String {
    match answer(session, "INFO FOR SEAL;").remove("state") {
        Some(Value::String(state)) => state,
        other => panic!("no state in the answer: {other:?}"),
    }
}

#[test]
fn a_store_that_never_held_a_vault_is_uninitialised() {
    let store = store();
    let mut session = Session::new(&store);
    let report = answer(&mut session, "INFO FOR SEAL;");
    assert_eq!(report.get("state"), Some(&Value::from("uninitialised")));
    assert_eq!(report.get("seals_at"), Some(&Value::None));
}

#[test]
fn the_state_follows_unseal_and_seal_and_says_when_it_ends() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();

    let report = answer(&mut session, "INFO FOR SEAL;");
    assert_eq!(report.get("state"), Some(&Value::from("unsealed")));
    assert!(
        matches!(report.get("seals_at"), Some(Value::Datetime(_))),
        "an unsealed store did not say when it seals: {report:?}"
    );
    assert_eq!(
        report.get("unseal_for"),
        Some(&Value::Duration(
            tessari_types::Duration::new(600, 0).unwrap()
        ))
    );

    session.run("SEAL VAULT;").unwrap();
    assert_eq!(state(&mut session), "sealed");
}

#[test]
fn an_unseal_past_its_period_reads_as_sealed() {
    let store = store();
    store.vault().last_for(std::time::Duration::ZERO);
    let mut session = Session::new(&store);
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();
    assert_eq!(state(&mut session), "sealed");
}

#[test]
fn any_signed_in_user_may_ask() {
    let store = store();
    let mut owner = Session::new(&store);
    owner
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE work;
             DEFINE USER root ROLE owner PASSWORD 'root-password';",
        )
        .unwrap();
    // The first user closes the store, so the second is declared by the owner.
    let mut root = Session::new(&store);
    root.sign_in("root", "root-password").unwrap();
    root.run("DEFINE USER ada ON prod.work ROLE viewer PASSWORD 'ada-password';")
        .unwrap();
    let mut viewer = Session::new(&store);
    viewer.sign_in("ada", "ada-password").unwrap();
    assert_eq!(state(&mut viewer), "uninitialised");

    let mut nobody = Session::new(&store);
    assert!(
        nobody.run("INFO FOR SEAL;").is_err(),
        "a closed store answered a caller who never signed in"
    );
}

#[test]
fn a_passphrase_guessed_three_times_wrong_is_made_to_wait() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();
    session.run("SEAL VAULT;").unwrap();

    for guess in ["guess one", "guess two", "guess three"] {
        let refused = session
            .run(&format!("UNSEAL VAULT WITH '{guess}';"))
            .expect_err("a wrong passphrase unsealed");
        assert!(
            !matches!(refused, tessari_session::Error::PassphraseThrottled),
            "throttled before the allowance was spent"
        );
    }
    // The right passphrase now waits too: the throttle bounds guesses at the
    // passphrase, and a caller who has missed three times is not told whether
    // the fourth was right.
    let refused = session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .expect_err("a fourth attempt was tried at once");
    assert!(
        matches!(refused, tessari_session::Error::PassphraseThrottled),
        "the fourth attempt was refused, but not by the throttle: {refused}"
    );
    assert_eq!(state(&mut session), "sealed");
}

// The dedicated surface (ADR-0092 D2): the same acts, reached without the
// passphrase ever being statement text.

fn answered(value: Value) -> std::collections::BTreeMap<String, Value> {
    match value {
        Value::Object(fields) => fields,
        other => panic!("expected an object, got {other:?}"),
    }
}

#[test]
fn the_surface_unseals_seals_and_reports_without_a_statement() {
    use tessari_session::{VaultAct, VaultTarget};
    let store = store();
    let mut session = Session::new(&store);

    let first = answered(
        session
            .vault(
                VaultTarget::Store,
                VaultAct::Unseal {
                    passphrase: PASSPHRASE,
                },
            )
            .unwrap(),
    );
    assert_eq!(first.get("state"), Some(&Value::from("unsealed")));
    assert_eq!(first.get("initialised"), Some(&Value::Bool(true)));

    let sealed = answered(session.vault(VaultTarget::Store, VaultAct::Seal).unwrap());
    assert_eq!(sealed.get("state"), Some(&Value::from("sealed")));

    let again = answered(
        session
            .vault(
                VaultTarget::Store,
                VaultAct::Unseal {
                    passphrase: PASSPHRASE,
                },
            )
            .unwrap(),
    );
    assert_eq!(again.get("state"), Some(&Value::from("unsealed")));
    assert_eq!(
        again.get("initialised"),
        None,
        "a second unseal said it initialised"
    );

    let status = answered(session.vault(VaultTarget::Store, VaultAct::Status).unwrap());
    assert_eq!(status.get("state"), Some(&Value::from("unsealed")));
}

#[test]
fn the_surface_asks_the_authority_the_statement_asks() {
    use tessari_session::{VaultAct, VaultTarget};
    let store = store();
    let mut owner = Session::new(&store);
    owner
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE work;
             DEFINE USER root ROLE owner PASSWORD 'root-password';",
        )
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", "root-password").unwrap();
    root.run("DEFINE USER ada ON prod.work ROLE viewer PASSWORD 'ada-password';")
        .unwrap();

    let mut viewer = Session::new(&store);
    viewer.sign_in("ada", "ada-password").unwrap();
    assert!(
        viewer
            .vault(
                VaultTarget::Store,
                VaultAct::Unseal {
                    passphrase: PASSPHRASE
                }
            )
            .is_err(),
        "a viewer unsealed the store"
    );
    assert!(
        viewer.vault(VaultTarget::Store, VaultAct::Seal).is_err(),
        "a viewer sealed the store"
    );
    assert_eq!(
        answered(viewer.vault(VaultTarget::Store, VaultAct::Status).unwrap()).get("state"),
        Some(&Value::from("uninitialised"))
    );
    root.vault(
        VaultTarget::Store,
        VaultAct::Unseal {
            passphrase: PASSPHRASE,
        },
    )
    .unwrap();
}

// A passphrase change is a rekey (ADR-0092 D3): the same master key under a new
// passphrase, so every secret opens afterwards and the old passphrase does not.

const PLANTED: &str = "correct-horse-battery-staple-9f2b";

fn holding_a_secret(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
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
    session
}

fn revealed(session: &mut Session<'_>) -> Value {
    match session
        .run("REVEAL token FROM team:'github';")
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Value(Value::Object(mut fields)) => fields.remove("token").unwrap(),
        other => panic!("expected the revealed fields, got {other:?}"),
    }
}

#[test]
fn a_changed_passphrase_opens_every_secret_and_the_old_one_opens_nothing() {
    let store = store();
    let mut session = holding_a_secret(&store);
    session
        .run(&format!(
            "CHANGE VAULT PASSPHRASE FROM '{PASSPHRASE}' TO 'the new passphrase';"
        ))
        .unwrap();
    // Still unsealed: a rekey re-wraps the root and leaves the key in memory
    // where it was.
    assert_eq!(state(&mut session), "unsealed");

    session.run("SEAL VAULT;").unwrap();
    assert!(
        session
            .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
            .is_err(),
        "the old passphrase still unseals"
    );
    session
        .run("UNSEAL VAULT WITH 'the new passphrase';")
        .unwrap();
    assert_eq!(revealed(&mut session), Value::from(PLANTED));
}

#[test]
fn a_wrong_current_passphrase_changes_nothing() {
    let store = store();
    let mut session = holding_a_secret(&store);
    let refused = session
        .run("CHANGE VAULT PASSPHRASE FROM 'not it' TO 'the new passphrase';")
        .expect_err("a wrong current passphrase was accepted");
    assert!(!refused.to_string().contains("not it"), "{refused}");
    assert!(
        !refused.to_string().contains("the new passphrase"),
        "{refused}"
    );

    session.run("SEAL VAULT;").unwrap();
    session
        .run(&format!("UNSEAL VAULT WITH '{PASSPHRASE}';"))
        .unwrap();
    assert_eq!(revealed(&mut session), Value::from(PLANTED));
}

#[test]
fn a_store_with_no_root_has_no_passphrase_to_change() {
    let store = store();
    let mut session = Session::new(&store);
    let refused = session
        .run("CHANGE VAULT PASSPHRASE FROM 'a' TO 'b';")
        .expect_err("a passphrase was changed on a store that never had one");
    assert!(
        matches!(refused, tessari_session::Error::NoVaultRoot),
        "refused, but not as having no root: {refused}"
    );
    assert_eq!(state(&mut session), "uninitialised");
}

#[test]
fn the_surface_changes_the_passphrase_too() {
    use tessari_session::{VaultAct, VaultTarget};
    let store = store();
    let mut session = holding_a_secret(&store);
    let answer = answered(
        session
            .vault(
                VaultTarget::Store,
                VaultAct::Change {
                    current: PASSPHRASE,
                    new: "the new passphrase",
                },
            )
            .unwrap(),
    );
    assert_eq!(answer.get("state"), Some(&Value::from("unsealed")));
    session.vault(VaultTarget::Store, VaultAct::Seal).unwrap();
    session
        .vault(
            VaultTarget::Store,
            VaultAct::Unseal {
                passphrase: "the new passphrase",
            },
        )
        .unwrap();
    assert_eq!(revealed(&mut session), Value::from(PLANTED));
}

// A vault lists its record ids, a page at a time, and never a value
// (ADR-0092 D5).

fn listed(session: &mut Session<'_>, statement: &str) -> (Vec<Value>, Value) {
    let mut report = answer(session, statement);
    let Some(Value::Array(records)) = report.remove("records") else {
        panic!("no records in the answer: {report:?}");
    };
    (records, report.remove("next").unwrap_or(Value::Null))
}

#[test]
fn a_vault_lists_its_ids_in_pages_and_never_a_value() {
    let store = store();
    let mut session = holding_a_secret(&store);
    session
        .run(&format!(
            "CREATE team:'gitlab' = {{ token: '{PLANTED}' }};
             CREATE team:'aws' = {{ token: '{PLANTED}' }};"
        ))
        .unwrap();

    let (all, next) = listed(&mut session, "INFO FOR VAULT team RECORDS;");
    assert_eq!(
        all,
        vec![
            Value::from("aws"),
            Value::from("github"),
            Value::from("gitlab")
        ]
    );
    assert_eq!(
        next,
        Value::None,
        "a listing that held everything named a next page"
    );

    let (first, next) = listed(&mut session, "INFO FOR VAULT team RECORDS LIMIT 2;");
    assert_eq!(first, vec![Value::from("aws"), Value::from("github")]);
    assert_eq!(next, Value::from("github"));
    let (rest, next) = listed(
        &mut session,
        "INFO FOR VAULT team RECORDS AFTER team:'github' LIMIT 2;",
    );
    assert_eq!(rest, vec![Value::from("gitlab")]);
    assert_eq!(next, Value::None);

    let rendered = format!("{:?}", session.run("INFO FOR VAULT team RECORDS;").unwrap());
    assert!(
        !rendered.contains(PLANTED),
        "a listing carried a value: {rendered}"
    );
    // The answer without `RECORDS` is unchanged: fields, and no ids.
    assert!(!answer(&mut session, "INFO FOR VAULT team;").contains_key("records"));
}

#[test]
fn a_listing_is_asked_of_the_vault_it_names() {
    let store = store();
    let mut session = holding_a_secret(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;
             DEFINE USER root ROLE owner PASSWORD 'root-password';",
        )
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", "root-password").unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE work;
         DEFINE USER ada ON prod.work ROLE editor PASSWORD 'ada-password';
         GRANT read, write ON notes TO ada;",
    )
    .unwrap();
    let mut ada = Session::new(&store);
    ada.sign_in("ada", "ada-password").unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE work;").unwrap();
    assert!(
        ada.run("INFO FOR VAULT team RECORDS;").is_err(),
        "a user granted another table listed the vault"
    );
    root.run("GRANT read ON team TO ada;").unwrap();
    let (listed_ids, _) = listed(&mut ada, "INFO FOR VAULT team RECORDS;");
    assert_eq!(listed_ids, vec![Value::from("github")]);

    let refused = root
        .run("INFO FOR VAULT team RECORDS LIMIT 10001;")
        .expect_err("a page above the ceiling was answered");
    assert!(refused.to_string().contains("10000"), "{refused}");
}

#[test]
fn a_record_id_in_a_vault_listing_or_recipient_list_may_be_a_parameter() {
    let store = store();
    let mut session = holding_a_secret(&store);
    session
        .run(&format!(
            "CREATE team:'gitlab' = {{ token: '{PLANTED}' }};
             ADD RECIPIENT 'bob' TO team:'gitlab' KEY 0x0102;"
        ))
        .unwrap();
    let mut parameters = tessari_ql::Parameters::new();
    parameters.insert("after".to_owned(), Value::from("github"));
    parameters.insert("id".to_owned(), Value::from("gitlab"));

    let mut page = match session
        .run_with(
            "INFO FOR VAULT team RECORDS AFTER team:$after;",
            &parameters,
        )
        .unwrap()
        .pop()
    {
        Some(Outcome::Value(Value::Object(report))) => report,
        other => panic!("expected a page, got {other:?}"),
    };
    assert_eq!(
        page.remove("records"),
        Some(Value::Array(vec![Value::from("gitlab")]))
    );

    let listed = session
        .run_with("INFO FOR RECIPIENTS OF team:$id;", &parameters)
        .unwrap();
    assert!(format!("{listed:?}").contains("bob"), "{listed:?}");
}
