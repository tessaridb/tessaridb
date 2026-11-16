//! The wire transaction block: BEGIN/COMMIT/ROLLBACK walk the connection's
//! state machine, the readiness carries I/T/E, a failure inside a block
//! aborts it, and a multi-statement query runs as one implicit block.

use super::{Client, SETUP, Seen, refused, rows, serving_with};

#[tokio::test]
async fn a_block_holds_the_readiness_until_it_closes() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client.query("BEGIN").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "BEGIN")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'T'))),
        "the readiness carries the open block: {seen:?}"
    );

    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'T'))),
        "a statement inside keeps the block open: {seen:?}"
    );

    let seen = client.query("COMMIT").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "COMMIT")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_verb_outside_a_block_refuses_by_name() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    for verb in ["COMMIT", "ROLLBACK"] {
        let seen = client.query(verb).await;
        let (code, _) = refused(&seen).unwrap_or_default();
        assert_eq!(code, "25P01", "{verb}: {seen:?}");
        assert!(
            seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
            "{seen:?}"
        );
    }
}

#[tokio::test]
async fn a_failure_inside_aborts_the_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN").await;
    let seen = client.query("SELECT FROM").await;
    assert!(refused(&seen).is_some(), "{seen:?}");
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'E'))),
        "the aborted block rides E: {seen:?}"
    );

    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    let (code, _) = refused(&seen).unwrap_or_default();
    assert_eq!(
        code, "25P02",
        "nothing runs inside an aborted block: {seen:?}"
    );

    // COMMIT of an aborted block answers ROLLBACK, as the dialect does.
    let seen = client.query("COMMIT").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
    let seen = client.query("SELECT name FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen), vec![vec![Some("first".to_owned())]]);
}

#[tokio::test]
async fn rollback_closes_an_open_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    client.query("BEGIN").await;
    let seen = client.query("ROLLBACK").await;
    assert!(
        seen.iter()
            .any(|m| matches!(m, Seen::Command(t) if t == "ROLLBACK")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_multi_statement_query_runs_as_one_block() {
    let (address, _stop, _db) = serving_with(SETUP).await;
    let mut client = Client::to(&address).await;
    client.start(None, "app.main", None).await;

    let seen = client
        .query("SELECT name FROM orders LIMIT 1; SELECT band FROM orders LIMIT 1")
        .await;
    assert_eq!(
        rows(&seen),
        vec![vec![Some("first".to_owned())], vec![Some("1".to_owned())]],
        "both statements answer, in order: {seen:?}"
    );
    assert!(
        seen.iter().any(|m| matches!(m, Seen::Ready(b'I'))),
        "the implicit block closed inside the run: {seen:?}"
    );

    // Two bare projections in one block hit the store's own rule — a script
    // answers with one value — and its words travel.
    let seen = client.query("SELECT 1; SELECT 2").await;
    let (code, message) = refused(&seen).unwrap_or_default();
    assert_eq!(code, "42601", "{seen:?}");
    assert!(
        message.contains("one value"),
        "the store's own contract speaks: {seen:?}"
    );

    // One refusing statement refuses the whole block: the first statement's
    // rows never leak.
    let seen = client.query("SELECT name FROM orders; SELECT FROM").await;
    assert!(refused(&seen).is_some(), "{seen:?}");
    assert!(
        rows(&seen).is_empty(),
        "no half-answered rows from a refused block: {seen:?}"
    );
}
