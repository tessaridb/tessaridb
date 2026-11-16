//! The tenancy mapping (the D5 amendment): where a connection lands when the
//! startup names a namespace, when it names none, and how it moves afterwards.

use super::{Client, SETUP, Seen, params, rows, serving_adjusted, serving_with};

/// A second namespace with a table the first one does not have.
const OTHER: &str = "DEFINE NAMESPACE other; USE NAMESPACE other; \
     DEFINE DATABASE main; USE DATABASE main; \
     DEFINE TABLE notes SCHEMALESS; CREATE notes:1 = { body: 'elsewhere' };";

#[tokio::test]
async fn a_bare_database_name_reads_inside_the_default_namespace() {
    let (address, _stop, _db) = serving_adjusted(&format!("{SETUP} {OTHER}"), |node| {
        node.defaulting_namespace("app")
    })
    .await;
    let mut client = Client::to(&address).await;
    let seen = client.start(None, "main", None).await;
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "a bare database name lands in the default namespace: {seen:?}"
    );
    let seen = client.query("SELECT body FROM notes").await;
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Error(_, _))),
        "the default namespace is app, not other: {seen:?}"
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
}

#[tokio::test]
async fn the_namespace_pair_and_the_options_string_name_the_tenancy() {
    for pairs in [
        vec![("database", "main"), ("namespace", "app")],
        vec![("database", "main"), ("options", "-c namespace=app")],
        vec![("database", "main"), ("options", "-cnamespace=app")],
    ] {
        let (address, _stop, _db) = serving_with(SETUP).await;
        let mut client = Client::to(&address).await;
        let seen = client.start_pairs(&pairs, None).await;
        assert!(
            seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
            "{pairs:?}: {seen:?}"
        );
        let seen = client.query("SELECT name FROM orders LIMIT 1").await;
        assert_eq!(
            rows(&seen),
            vec![vec![Some("first".to_owned())]],
            "{pairs:?}: {seen:?}"
        );
    }
}

#[tokio::test]
async fn the_explicit_database_field_wins_over_the_pair() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client
        .start_pairs(&[("database", "app.main"), ("namespace", "nowhere")], None)
        .await;
    assert!(seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))));
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
}

#[tokio::test]
async fn no_database_at_all_follows_the_dialects_convention() {
    // A bare startup lands in the default namespace; the database takes the
    // user's name when one was presented, `main` when none was. The first
    // holds here: no user, no database — the connection reads app.main.
    let (address, _stop, _db) =
        serving_adjusted(SETUP, |node| node.defaulting_namespace("app")).await;
    let mut client = Client::to(&address).await;
    let seen = client.start_pairs(&[], None).await;
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
    // The user-name half: a presented user names the database, and signs in.
    let setup = "USE NAMESPACE app; DEFINE DATABASE scott; USE DATABASE scott; \
                 DEFINE USER scott ON app.scott ROLE viewer PASSWORD 'pw';";
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {setup}")).await;
    let mut client = Client::to(&address).await;
    let seen = client.start_pairs(&[("user", "scott")], Some("pw")).await;
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
    let seen = client.query("SHOW search_path").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("public".to_owned())]],
        "no channel named a namespace, so the built-in default answers: {seen:?}"
    );
}

#[tokio::test]
async fn the_builtin_default_is_the_dialects_public_schema() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client.start(None, "main", None).await;
    assert!(seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))));
    let seen = client.query("SHOW search_path").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("public".to_owned())]],
        "no channel named a namespace, so the built-in default answers: {seen:?}"
    );
}

#[tokio::test]
async fn set_search_path_moves_the_connection() {
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {OTHER}")).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client.query("SET search_path TO other").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SET")),
        "{seen:?}"
    );
    let seen = client.query("SHOW search_path").await;
    assert_eq!(rows(&seen), vec![vec![Some("other".to_owned())]]);
    let seen = client.query("SELECT body FROM notes").await;
    assert_eq!(rows(&seen), vec![vec![Some("elsewhere".to_owned())]]);

    // And back: the store's own words reach the first namespace again.
    let seen = client.query("SET search_path = app").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SET"))
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
}

#[tokio::test]
async fn other_session_settings_refuse_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client.query("SET client_encoding TO 'LATIN1'").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    let seen = client.query("SHOW server_version").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
}

#[tokio::test]
async fn search_path_is_reported_at_startup() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    let seen = client.start(None, "app.main", None).await;
    let reported = params(&seen);
    assert!(
        reported
            .iter()
            .any(|(name, value)| name == "search_path" && value == "app"),
        "{reported:?}"
    );
}
