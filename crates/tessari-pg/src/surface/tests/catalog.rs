//! The catalog over the socket: the captured driver query answers through
//! both protocols, the relation set lists itself, and what the subset does
//! not speak refuses by name with the session surviving.

use super::{Client, SETUP, Seen, refused, rows, serving_with};

/// The query the dialect's Rust driver sends to resolve an unspecified
/// parameter type (captured from the wire, defer list §1).
const CAPTURED: &str = "SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, \
     t.typbasetype, n.nspname, t.typrelid \
     FROM pg_catalog.pg_type t \
     LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
     INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
     WHERE t.oid = $1";

#[tokio::test]
async fn the_captured_driver_query_answers_over_the_simple_protocol() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client.query(&CAPTURED.replace("$1", "25")).await;
    assert!(
        seen.iter().any(|m| matches!(
            m,
            Seen::RowDesc(cols)
                if cols
                    == &[
                        "typname".to_owned(),
                        "typtype".to_owned(),
                        "typelem".to_owned(),
                        "rngsubtype".to_owned(),
                        "typbasetype".to_owned(),
                        "nspname".to_owned(),
                        "typrelid".to_owned()
                    ]
        )),
        "{seen:?}"
    );
    assert_eq!(
        rows(&seen),
        vec![vec![
            Some("text".to_owned()),
            Some("b".to_owned()),
            Some("0".to_owned()),
            None,
            Some("0".to_owned()),
            Some("pg_catalog".to_owned()),
            Some("0".to_owned())
        ]],
        "{seen:?}"
    );
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    stop.cancel();
}

#[tokio::test]
async fn the_captured_driver_query_answers_over_the_extended_protocol() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.parse("", CAPTURED, &[]).await;
    client.bind("", "", &[], &[Some(b"25".to_vec())], &[]).await;
    client.describe(b'P', "").await;
    client.execute("", 0).await;
    client.sync().await;

    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(client.message().await);
    }
    assert!(
        seen.contains(&Seen::ParseComplete),
        "Parse answers ParseComplete: {seen:?}"
    );
    assert!(
        seen.contains(&Seen::BindComplete),
        "Bind answers BindComplete: {seen:?}"
    );
    assert_eq!(
        rows(&seen),
        vec![vec![
            Some("text".to_owned()),
            Some("b".to_owned()),
            Some("0".to_owned()),
            None,
            Some("0".to_owned()),
            Some("pg_catalog".to_owned()),
            Some("0".to_owned())
        ]],
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 1")),
        "{seen:?}"
    );
    assert!(seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))));
    stop.cancel();
}

#[tokio::test]
async fn the_driver_resolution_flow_answers() {
    // The dialect's Rust driver: Describe(statement), a binary-typed Bind,
    // no Describe(portal) — Execute owes only rows and completion.
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.parse("", CAPTURED, &[]).await;
    client.describe(b'S', "").await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(client.message().await);
    }
    assert!(
        seen.contains(&Seen::ParamDesc(vec![26])),
        "Describe(statement) answers the inferred oid: {seen:?}"
    );

    client
        .bind("", "", &[1], &[Some(0_i32.to_be_bytes().to_vec())], &[])
        .await;
    client.execute("", 0).await;
    client.sync().await;
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(client.message().await);
    }
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 0")),
        "type 0 lives in no catalog: {seen:?}"
    );
    assert!(seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))));
    stop.cancel();
}

#[tokio::test]
async fn the_relation_set_lists_itself() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client
        .query("SELECT t.typname FROM pg_catalog.pg_type t WHERE t.typtype = 'b' LIMIT 2")
        .await;
    assert_eq!(rows(&seen).len(), 2, "{seen:?}");
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "SELECT 2")),
        "{seen:?}"
    );
    stop.cancel();
}

#[tokio::test]
async fn an_unspoken_catalog_shape_refuses_and_the_session_survives() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client
        .query("SELECT t.typtype FROM pg_catalog.pg_type t GROUP BY t.typtype")
        .await;
    let (code, _) = refused(&seen).expect("the refusal");
    assert_eq!(code, "42601");
    assert!(matches!(seen.last(), Some(Seen::Ready(b'I'))));
    // The session is whole: a plain read still answers.
    let seen = client.query("SELECT id FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen).len(), 1, "{seen:?}");
    stop.cancel();
}

#[tokio::test]
async fn a_mixed_batch_refuses_by_name() {
    let (address, stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;
    let seen = client
        .query("SELECT t.typname FROM pg_catalog.pg_type t; SELECT id FROM orders")
        .await;
    let (code, message) = refused(&seen).expect("the refusal");
    assert_eq!(code, "0A000");
    assert!(
        message.contains("alone"),
        "the refusal names the limit: {message}"
    );
    stop.cancel();
}
