//! `DEFINE AI PROVIDER` (G102 C8 sub-wave A): the provider as a catalog
//! object whose key is named as a vault secret and never written.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_session::{RunNow, Session};

use super::materialized_views::store;

fn ready(store: &tessari_storage::Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             UNSEAL VAULT WITH 'an operator passphrase';\n\
             DEFINE VAULT secrets;",
        )
        .unwrap();
    session
}

const DECLARE: &str = "DEFINE AI PROVIDER anthropic \
    URL 'https://api.example.test/v1/messages' MODEL 'a-model' \
    KEY VAULT secrets FIELD anthropic_key BUDGET 1000 PER 1h TIMEOUT 10s;";

#[test]
fn a_provider_declares_and_drops() {
    let store = store();
    let mut session = ready(&store);
    session.run_now(DECLARE).unwrap();
    // A repeat declaration refuses by name; IF NOT EXISTS accepts it.
    assert!(
        session.run_now(DECLARE).is_err(),
        "a repeat declaration was accepted"
    );
    session
        .run_now(&DECLARE.replace("DEFINE AI PROVIDER", "DEFINE AI PROVIDER IF NOT EXISTS"))
        .unwrap();
    session.run_now("DROP AI PROVIDER anthropic;").unwrap();
    assert!(
        session.run_now("DROP AI PROVIDER anthropic;").is_err(),
        "dropping what is gone was accepted"
    );
}

#[test]
fn the_sealing_rules_hold_at_declare_time() {
    let store = store();
    let mut session = ready(&store);
    // A vault the database does not hold: the key has nowhere sealed to
    // live, so the provider refuses to exist.
    let refused = session
        .run_now(
            "DEFINE AI PROVIDER ghost URL 'https://api.example.test/v1' MODEL 'm' \
             KEY VAULT nowhere FIELD key BUDGET 10 PER 1h TIMEOUT 5s;",
        )
        .unwrap_err();
    assert!(
        refused.to_string().contains("no sealed key"),
        "the refusal did not name the sealing: {refused}"
    );
    // Plaintext never carries a key.
    assert!(
        session
            .run_now(
                "DEFINE AI PROVIDER plain URL 'http://api.example.test/v1' MODEL 'm' \
                 KEY VAULT secrets FIELD key BUDGET 10 PER 1h TIMEOUT 5s;",
            )
            .is_err(),
        "a plaintext endpoint was accepted"
    );
    // Every clause is required: TIMEOUT names no default.
    assert!(
        session
            .run_now(
                "DEFINE AI PROVIDER bare URL 'https://api.example.test/v1' MODEL 'm' \
                 KEY VAULT secrets FIELD key BUDGET 10 PER 1h;",
            )
            .is_err(),
        "a provider with no TIMEOUT was accepted"
    );
}
