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
async fn unperformable_session_values_refuse_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // G091: accept-and-report stands, and the values the port cannot
    // honestly perform still refuse — another encoding is one of them.
    let seen = client.query("SET client_encoding TO 'LATIN1'").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
}

#[tokio::test]
async fn session_settings_accept_and_report_within_their_honest_values() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // A value the port cannot honestly perform still refuses by name.
    let seen = client.query("SET client_encoding TO 'LATIN1'").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    // The performable value accepts and answers back — it is also the startup
    // value, so the dialect sends no ParameterStatus for an unchanged value.
    let seen = client.query("SET client_encoding TO 'UTF8'").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SET")),
        "the SET answers: {seen:?}"
    );
    assert!(
        !seen.iter().any(|m| matches!(m, Seen::Param(..))),
        "an unchanged value echoes nothing: {seen:?}"
    );
    let seen = client.query("SHOW client_encoding").await;
    assert_eq!(rows(&seen), vec![vec![Some("UTF8".to_owned())]]);
    // SHOW on a read-only parameter answers the reported value; SET refuses.
    let seen = client.query("SHOW server_version").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::RowDesc(cols) if cols == &["server_version".to_owned()])),
        "SHOW answers what the greeting reported: {seen:?}"
    );
    let seen = client.query("SET server_version TO '9.0'").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    stop.cancel();
}

#[tokio::test]
async fn the_reported_set_round_trips_and_resets() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // SET SESSION is SET; a SET echoes the new value in ParameterStatus.
    let seen = client
        .query("SET SESSION application_name TO 'the pager'")
        .await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Param(name, value) if name == "application_name" && value == "the pager")),
        "{seen:?}"
    );
    let seen = client.query("SHOW application_name").await;
    assert_eq!(rows(&seen), vec![vec![Some("the pager".to_owned())]]);
    // SET TIME ZONE and SET NAMES are the dialect's own spellings.
    let seen = client.query("SET TIME ZONE 'UTC'").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SET")),
        "{seen:?}"
    );
    let seen = client.query("SET NAMES 'UTF8'").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SET")),
        "{seen:?}"
    );
    // RESET returns the startup-reported value; RESET ALL restores all of it.
    let seen = client.query("RESET application_name").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "RESET")),
        "{seen:?}"
    );
    let seen = client.query("SHOW application_name").await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some(String::new())]],
        "the unnamed default answers: {seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn the_unspeakable_session_words_refuse_by_name() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    // Values outside the honest set: another zone, another date style, a
    // conforming-strings-off that cannot be performed.
    for query in [
        "SET TimeZone TO 'Europe/Paris'",
        "SET DateStyle TO 'German'",
        "SET standard_conforming_strings TO 'off'",
        "SET extra_float_digits TO 'many'",
    ] {
        let seen = client.query(query).await;
        let (code, _) = super::refused(&seen).unwrap_or_default();
        assert_eq!(code, "0A000", "{query}: {seen:?}");
    }
    // SET LOCAL's transaction-scoped effect is not spoken.
    let seen = client.query("SET LOCAL application_name TO 'brief'").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    // A setting the greeting never reported refuses by name.
    let seen = client.query("SET work_mem TO '64MB'").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    let seen = client.query("SHOW work_mem").await;
    let (code, _) = super::refused(&seen).unwrap_or_default();
    assert_eq!(code, "0A000", "{seen:?}");
    // extra_float_digits accepts what it renders exact anyway.
    let seen = client.query("SET extra_float_digits TO -3").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SET")),
        "{seen:?}"
    );
    stop.cancel();
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
